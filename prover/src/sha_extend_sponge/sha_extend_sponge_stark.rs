use std::marker::PhantomData;
use std::borrow::Borrow;
use itertools::Itertools;
use plonky2::field::extension::{Extendable, FieldExtension};
use plonky2::field::packed::PackedField;
use plonky2::field::polynomial::PolynomialValues;
use plonky2::field::types::Field;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::ext_target::ExtensionTarget;
use plonky2::plonk::circuit_builder::CircuitBuilder;
use crate::constraint_consumer::{ConstraintConsumer, RecursiveConstraintConsumer};
use crate::evaluation_frame::{StarkEvaluationFrame, StarkFrame};
use crate::memory::segments::Segment;
use crate::sha_extend::logic::{get_input_range, from_be_bits_to_u32, from_u32_to_be_bits};
use crate::sha_extend_sponge::columns::{ShaExtendSpongeColumnsView, NUM_EXTEND_INPUT, NUM_SHA_EXTEND_SPONGE_COLUMNS};
use crate::sha_extend_sponge::logic::{diff_address_ext_circuit_constraint, round_increment_ext_circuit_constraint};
use crate::stark::Stark;
use crate::util::trace_rows_to_poly_values;
use crate::witness::memory::MemoryAddress;

pub const NUM_ROUNDS: usize = 48;

pub(crate) struct  ShaExtendSpongeOp {
    /// The base address at which inputs are read
    pub(crate) base_address: Vec<MemoryAddress>,

    /// The timestamp at which inputs are read and output are written (same for both).
    pub(crate) timestamp: usize,

    /// The input that was read.
    /// Values: w_i_minus_15, w_i_minus_2, w_i_minus_16, w_i_minus_7 in big-endian order.
    pub(crate) input: Vec<u32>,

    /// The index of round
    pub(crate) i: u32,

    /// The base address at which the output is written.
    pub(crate) output_address: MemoryAddress,
}

#[derive(Copy, Clone, Default)]
pub struct ShaExtendSpongeStark<F, const D: usize> {
    f: PhantomData<F>,
}

impl<F: RichField + Extendable<D>, const D: usize> ShaExtendSpongeStark<F, D> {
    pub(crate) fn generate_trace(
        &self,
        operations: Vec<ShaExtendSpongeOp>,
        min_rows: usize,
    ) -> Vec<PolynomialValues<F>> {
        // Generate the witness row-wise.
        let trace_rows = self.generate_trace_rows(operations, min_rows);

        trace_rows_to_poly_values(trace_rows)
    }

    fn generate_trace_rows(
        &self,
        operations: Vec<ShaExtendSpongeOp>,
        min_rows: usize,
    ) -> Vec<[F; NUM_SHA_EXTEND_SPONGE_COLUMNS]> {
        let base_len = operations.len();
        let mut rows = Vec::with_capacity(base_len.max(min_rows).next_power_of_two());
        for op in operations {
            rows.push(self.generate_rows_for_op(op).into());
        }

        let padded_rows = rows.len().max(min_rows).next_power_of_two();
        for _ in rows.len()..padded_rows {
            rows.push(ShaExtendSpongeColumnsView::default().into());
        }

        rows
    }

    fn generate_rows_for_op(&self, op: ShaExtendSpongeOp) -> ShaExtendSpongeColumnsView<F>{
        let mut row = ShaExtendSpongeColumnsView::default();
        row.timestamp = F::from_canonical_usize(op.timestamp);
        row.round = [F::ZEROS; 48];
        row.round[op.i as usize] = F::ONE;

        row.context = F::from_canonical_usize(op.base_address[0].context);
        row.segment = F::from_canonical_usize(op.base_address[Segment::Code as usize].segment);
        let virt = (0..op.input.len() / 32)
            .map(|i| op.base_address[i].virt)
            .collect_vec();
        let virt: [usize; 4] = virt.try_into().unwrap();
        row.input_virt = virt.map(F::from_canonical_usize);
        row.output_virt = F::from_canonical_usize(op.output_address.virt);

        row.w_i_minus_15 = op.input[get_input_range(0)]
            .iter().map(|&x| F::from_canonical_u32(x)).collect::<Vec<_>>().try_into().unwrap();
        row.w_i_minus_2 = op.input[get_input_range(1)]
            .iter().map(|&x| F::from_canonical_u32(x)).collect::<Vec<_>>().try_into().unwrap();
        row.w_i_minus_16 = op.input[get_input_range(2)]
            .iter().map(|&x| F::from_canonical_u32(x)).collect::<Vec<_>>().try_into().unwrap();
        row.w_i_minus_7 = op.input[get_input_range(3)]
            .iter().map(|&x| F::from_canonical_u32(x)).collect::<Vec<_>>().try_into().unwrap();

        row.w_i = self.compute_w_i(&mut row);
        row
    }

    fn compute_w_i(&self, row: &mut ShaExtendSpongeColumnsView<F>) -> [F; 32] {
        let w_i_minus_15 = from_be_bits_to_u32(row.w_i_minus_15);
        let w_i_minus_2 = from_be_bits_to_u32(row.w_i_minus_2);
        let w_i_minus_16 = from_be_bits_to_u32(row.w_i_minus_16);
        let w_i_minus_7 = from_be_bits_to_u32(row.w_i_minus_7);
        let s0 = w_i_minus_15.rotate_right(7) ^ w_i_minus_15.rotate_right(18) ^ (w_i_minus_15 >> 3);
        let s1 = w_i_minus_2.rotate_right(17) ^ w_i_minus_2.rotate_right(19) ^ (w_i_minus_2 >> 10);
        let w_i_u32 = s1
            .wrapping_add(w_i_minus_16)
            .wrapping_add(s0)
            .wrapping_add(w_i_minus_7);

        let w_i_bin = from_u32_to_be_bits(w_i_u32);
        w_i_bin.iter().map(|&x| F::from_canonical_u32(x)).collect::<Vec<_>>().try_into().unwrap()
    }
}

impl<F: RichField + Extendable<D>, const D: usize> Stark<F, D> for ShaExtendSpongeStark<F, D> {
    type EvaluationFrame<FE, P, const D2: usize>
    = StarkFrame<P, NUM_SHA_EXTEND_SPONGE_COLUMNS>
    where
        FE: FieldExtension<D2, BaseField=F>,
        P: PackedField<Scalar=FE>;

    type EvaluationFrameTarget = StarkFrame<ExtensionTarget<D>, NUM_SHA_EXTEND_SPONGE_COLUMNS>;

    fn eval_packed_generic<FE, P, const D2: usize>(
        &self,
        vars: &Self::EvaluationFrame<FE, P, D2>,
        yield_constr: &mut ConstraintConsumer<P>
    ) where
        FE: FieldExtension<D2, BaseField=F>,
        P: PackedField<Scalar=FE>
    {

        let local_values: &[P; NUM_SHA_EXTEND_SPONGE_COLUMNS] =
            vars.get_local_values().try_into().unwrap();
        let local_values: &ShaExtendSpongeColumnsView<P> = local_values.borrow();
        let next_values: &[P; NUM_SHA_EXTEND_SPONGE_COLUMNS] =
            vars.get_next_values().try_into().unwrap();
        let next_values: &ShaExtendSpongeColumnsView<P> = next_values.borrow();

        // check the binary form
        for i in 0..32 {
            yield_constr.constraint(local_values.w_i_minus_15[i] * (local_values.w_i_minus_15[i] - P::ONES));
            yield_constr.constraint(local_values.w_i_minus_2[i] * (local_values.w_i_minus_2[i] - P::ONES));
            yield_constr.constraint(local_values.w_i_minus_16[i] * (local_values.w_i_minus_16[i] - P::ONES));
            yield_constr.constraint(local_values.w_i_minus_7[i] * (local_values.w_i_minus_7[i] - P::ONES));
            yield_constr.constraint(local_values.w_i[i] * (local_values.w_i[i] - P::ONES));
        }

        // check the round
        for i in 0..NUM_ROUNDS {
            yield_constr.constraint(local_values.round[i] * (local_values.round[i] - P::ONES));
        }

        // check the filter
        let is_final = local_values.round[NUM_ROUNDS - 1];
        yield_constr.constraint(is_final * (is_final - P::ONES));
        let not_final = P::ONES - is_final;

        let sum_round_flags = (0..NUM_ROUNDS)
            .map(|i| local_values.round[i])
            .sum::<P>();

        // If this is not the final step or a padding row,
        // the local and next timestamps must match.
        yield_constr.constraint(
            sum_round_flags * not_final * (next_values.timestamp - local_values.timestamp),
        );

        // If this is not the final step or a padding row,
        // round index should be increased by one

        let local_round_index = (0..NUM_ROUNDS)
            .map(|i| local_values.round[i] * FE::from_canonical_u32(i as u32))
            .sum::<P>();
        let next_round_index = (0..NUM_ROUNDS)
            .map(|i| next_values.round[i] * FE::from_canonical_u32(i as u32))
            .sum::<P>();
        yield_constr.constraint(
            sum_round_flags * not_final * (next_round_index - local_round_index - P::ONES)
        );

        // If this is not the final step or a padding row,
        // input and output addresses should be increased by 4 each
        (0..NUM_EXTEND_INPUT).for_each(|i| {
            yield_constr.constraint(
                sum_round_flags * not_final * (next_values.input_virt[i] - local_values.input_virt[i] - FE::from_canonical_u32(4))
            );
        });
        yield_constr.constraint(
            sum_round_flags * not_final * (next_values.output_virt - local_values.output_virt - FE::from_canonical_u32(4))
        );

        // If it's not the padding row, check the virtual addresses
        // The list of input addresses are: w[i-15], w[i-2], w[i-16], w[i-7]

        // add_w[i-15] = add_w[i-16] + 4
        yield_constr.constraint(
            sum_round_flags * (local_values.input_virt[0] - local_values.input_virt[2] - FE::from_canonical_u32(4))
        );
        // add_w[i-2] = add_w[i-16] + 56
        yield_constr.constraint(
            sum_round_flags * (local_values.input_virt[1] - local_values.input_virt[2] - FE::from_canonical_u32(56))
        );
        // add_w[i-7] = add_w[i-16] + 36
        yield_constr.constraint(
            sum_round_flags * (local_values.input_virt[3] - local_values.input_virt[2] - FE::from_canonical_u32(36))
        );
        // add_w[i] = add_w[i-16] + 64
        yield_constr.constraint(
            sum_round_flags * (local_values.output_virt - local_values.input_virt[2] - FE::from_canonical_u32(64))
        );
    }

    fn eval_ext_circuit(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        vars: &Self::EvaluationFrameTarget,
        yield_constr: &mut RecursiveConstraintConsumer<F, D>
    ) {

        let local_values: &[ExtensionTarget<D>; NUM_SHA_EXTEND_SPONGE_COLUMNS] =
            vars.get_local_values().try_into().unwrap();
        let local_values: &ShaExtendSpongeColumnsView<ExtensionTarget<D>> = local_values.borrow();
        let next_values: &[ExtensionTarget<D>; NUM_SHA_EXTEND_SPONGE_COLUMNS] =
            vars.get_next_values().try_into().unwrap();
        let next_values: &ShaExtendSpongeColumnsView<ExtensionTarget<D>> = next_values.borrow();

        let one_ext = builder.one_extension();
        let four_ext = builder.constant_extension(F::Extension::from_canonical_u32(4));

        // check the binary form
        for i in 0..32 {
            let constraint = builder.mul_sub_extension(
                local_values.w_i_minus_15[i], local_values.w_i_minus_15[i], local_values.w_i_minus_15[i]);
            yield_constr.constraint(builder, constraint);

            let constraint = builder.mul_sub_extension(
                local_values.w_i_minus_2[i], local_values.w_i_minus_2[i], local_values.w_i_minus_2[i]);
            yield_constr.constraint(builder, constraint);

            let constraint = builder.mul_sub_extension(
                local_values.w_i_minus_16[i], local_values.w_i_minus_16[i], local_values.w_i_minus_16[i]);
            yield_constr.constraint(builder, constraint);

            let constraint = builder.mul_sub_extension(
                local_values.w_i_minus_7[i], local_values.w_i_minus_7[i], local_values.w_i_minus_7[i]);
            yield_constr.constraint(builder, constraint);

            let constraint = builder.mul_sub_extension(
                local_values.w_i[i], local_values.w_i[i], local_values.w_i[i]);
            yield_constr.constraint(builder, constraint);
        }

        // check the round
        for i in 0..NUM_ROUNDS {
            let constraint = builder.mul_sub_extension(
                local_values.round[i], local_values.round[i], local_values.round[i]
            );
            yield_constr.constraint(builder, constraint);
        }

        // check the filter
        let is_final = local_values.round[NUM_ROUNDS - 1];
        let constraint = builder.mul_sub_extension(is_final, is_final, is_final);
        yield_constr.constraint(builder, constraint);
        let not_final = builder.sub_extension(one_ext, is_final);

        let sum_round_flags =
            builder.add_many_extension((0..NUM_ROUNDS).map(|i| local_values.round[i]));

        // If this is not the final step or a padding row,
        // the local and next timestamps must match.
        let diff = builder.sub_extension(next_values.timestamp, local_values.timestamp);
        let constraint = builder.mul_many_extension([sum_round_flags, not_final, diff]);
        yield_constr.constraint(builder, constraint);

        // If this is not the final step or a padding row,
        // round index should be increased by one

        let round_increment = round_increment_ext_circuit_constraint(
            builder,
            local_values.round,
            next_values.round
        );
        let constraint = builder.mul_many_extension(
            [sum_round_flags, not_final, round_increment]
        );
        yield_constr.constraint(builder, constraint);

        // If this is not the final step or a padding row,
        // input and output addresses should be increased by 4 each
        (0..NUM_EXTEND_INPUT).for_each(|i| {

            let increment = builder.sub_extension(next_values.input_virt[i], local_values.input_virt[i]);
            let address_increment = builder.sub_extension(increment, four_ext);
            let constraint = builder.mul_many_extension(
                [sum_round_flags, not_final, address_increment]
            );
            yield_constr.constraint(builder, constraint);
        });

        let increment = builder.sub_extension(next_values.output_virt, local_values.output_virt);
        let address_increment = builder.sub_extension(increment, four_ext);
        let constraint = builder.mul_many_extension(
            [sum_round_flags, not_final, address_increment]
        );
        yield_constr.constraint(builder, constraint);


        // If it's not the padding row, check the virtual addresses
        // The list of input addresses are: w[i-15], w[i-2], w[i-16], w[i-7]

        // add_w[i-15] = add_w[i-16] + 4
        let constraint = diff_address_ext_circuit_constraint(
            builder,
            sum_round_flags,
            local_values.input_virt[0],
            local_values.input_virt[2],
            4
        );
        yield_constr.constraint(builder, constraint);

        // add_w[i-2] = add_w[i-16] + 56
        let constraint = diff_address_ext_circuit_constraint(
            builder,
            sum_round_flags,
            local_values.input_virt[1],
            local_values.input_virt[2],
            56
        );
        yield_constr.constraint(builder, constraint);

        // add_w[i-7] = add_w[i-16] + 36
        let constraint = diff_address_ext_circuit_constraint(
            builder,
            sum_round_flags,
            local_values.input_virt[3],
            local_values.input_virt[2],
            36
        );
        yield_constr.constraint(builder, constraint);

        // add_w[i] = add_w[i-16] + 64
        let constraint = diff_address_ext_circuit_constraint(
            builder,
            sum_round_flags,
            local_values.output_virt,
            local_values.input_virt[2],
            64
        );
        yield_constr.constraint(builder, constraint);
    }

    fn constraint_degree(&self) -> usize {
        3
    }
}


#[cfg(test)]
mod test {
    use env_logger::{try_init_from_env, Env, DEFAULT_FILTER_ENV};
    use plonky2::field::goldilocks_field::GoldilocksField;
    use plonky2::field::polynomial::PolynomialValues;
    use plonky2::field::types::Field;
    use plonky2::fri::oracle::PolynomialBatch;
    use plonky2::iop::challenger::Challenger;
    use plonky2::plonk::config::{GenericConfig, PoseidonGoldilocksConfig};
    use plonky2::timed;
    use plonky2::util::timing::TimingTree;
    use crate::config::StarkConfig;
    use crate::cross_table_lookup::{Column, CtlData, CtlZData, Filter, GrandProductChallenge, GrandProductChallengeSet};
    use crate::memory::segments::Segment;
    use crate::prover::prove_single_table;
    use crate::sha_extend_sponge::sha_extend_sponge_stark::{ShaExtendSpongeOp, ShaExtendSpongeStark};
    use crate::stark_testing::{test_stark_circuit_constraints, test_stark_low_degree};
    use crate::witness::memory::MemoryAddress;

    fn to_be_bits(value: u32) -> [u32; 32] {
        let mut result = [0; 32];
        for i in 0..32 {
            result[i] = ((value >> i) & 1) as u32;
        }
        result
    }

    #[test]
    fn test_correction() -> Result<(), String> {
        const D: usize = 2;
        type F = GoldilocksField;

        type S = ShaExtendSpongeStark<F, D>;

        let mut input_values = vec![];
        input_values.extend((0..4).map(|i| to_be_bits(i as u32)));
        let input_values = input_values.into_iter().flatten().collect::<Vec<_>>();

        let op = ShaExtendSpongeOp {
            base_address: vec![MemoryAddress {
                context: 0,
                segment: Segment::Code as usize,
                virt: 4,
            }, MemoryAddress {
                context: 0,
                segment: Segment::Code as usize,
                virt: 56,
            }, MemoryAddress {
                context: 0,
                segment: Segment::Code as usize,
                virt: 0,
            }, MemoryAddress {
                context: 0,
                segment: Segment::Code as usize,
                virt: 36,
            }],
            timestamp: 0,
            input: input_values,
            i: 0,
            output_address: MemoryAddress {
                context: 0,
                segment: Segment::Code as usize,
                virt: 64,
            },
        };

        let stark = S::default();
        let row = stark.generate_rows_for_op(op);

        let w_i_bin = to_be_bits(40965);
        assert_eq!(row.w_i, w_i_bin.map(F::from_canonical_u32));

        Ok(())
    }

    #[test]
    fn test_stark_circuit() -> anyhow::Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type S = ShaExtendSpongeStark<F, D>;

        let stark = S::default();
        test_stark_circuit_constraints::<F, C, S, D>(stark)
    }

    #[test]
    fn test_stark_degree() -> anyhow::Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type S = ShaExtendSpongeStark<F, D>;

        let stark = S {
            f: Default::default(),
        };
        test_stark_low_degree(stark)
    }

    fn get_random_input() -> Vec<ShaExtendSpongeOp> {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = rand::random::<u32>();
        }
        for i in 16..64 {

            let w_i_minus_15 = w[i-15];
            let s0 = w_i_minus_15.rotate_right(7) ^ w_i_minus_15.rotate_right(18) ^ (w_i_minus_15 >> 3);

            // Read w[i-2].
            let w_i_minus_2 = w[i-2];
            // Compute `s1`.
            let s1 = w_i_minus_2.rotate_right(17) ^ w_i_minus_2.rotate_right(19) ^ (w_i_minus_2 >> 10);

            // Read w[i-16].
            let w_i_minus_16 = w[i-16];
            let w_i_minus_7 = w[i-7];

            // Compute `w_i`.
            w[i] = s1
                .wrapping_add(w_i_minus_16)
                .wrapping_add(s0)
                .wrapping_add(w_i_minus_7);
        }

        let mut addresses = vec![];
        for i in 0..64 {
            addresses.push(MemoryAddress{
                context: 0,
                segment: Segment::Code as usize,
                virt: i * 4
            });
        }

        let mut res = vec![];

        for i in 16..64 {
            let mut input_values = vec![];
            input_values.extend(to_be_bits(w[i - 15]));
            input_values.extend(to_be_bits(w[i - 2]));
            input_values.extend(to_be_bits(w[i - 16]));
            input_values.extend(to_be_bits(w[i - 7]));

            let op = ShaExtendSpongeOp {
                base_address: vec![addresses[i - 15], addresses[i - 2], addresses[i - 16], addresses[i - 7]],
                timestamp: 0,
                input: input_values,
                i: i as u32 - 16,
                output_address: addresses[i],
            };

            res.push(op);
        }

        res

    }
    #[test]
    fn sha_extend_sponge_benchmark() -> anyhow::Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type S = ShaExtendSpongeStark<F, D>;
        let stark = S::default();
        let config = StarkConfig::standard_fast_config();

        init_logger();

        let input = get_random_input();
        let mut timing = TimingTree::new("prove", log::Level::Debug);
        let trace_poly_values = stark.generate_trace(input, 8);

        // TODO: Cloning this isn't great; consider having `from_values` accept a reference,
        // or having `compute_permutation_z_polys` read trace values from the `PolynomialBatch`.
        let cloned_trace_poly_values = timed!(timing, "clone", trace_poly_values.clone());

        let trace_commitments = timed!(
            timing,
            "compute trace commitment",
            PolynomialBatch::<F, C, D>::from_values(
                cloned_trace_poly_values,
                config.fri_config.rate_bits,
                false,
                config.fri_config.cap_height,
                &mut timing,
                None,
            )
        );
        let degree = 1 << trace_commitments.degree_log;

        // Fake CTL data.
        let ctl_z_data = CtlZData {
            helper_columns: vec![PolynomialValues::zero(degree)],
            z: PolynomialValues::zero(degree),
            challenge: GrandProductChallenge {
                beta: F::ZERO,
                gamma: F::ZERO,
            },
            columns: vec![],
            filter: vec![Some(Filter::new_simple(Column::constant(F::ZERO)))],
        };
        let ctl_data = CtlData {
            zs_columns: vec![ctl_z_data.clone(); config.num_challenges],
        };

        prove_single_table(
            &stark,
            &config,
            &trace_poly_values,
            &trace_commitments,
            &ctl_data,
            &GrandProductChallengeSet {
                challenges: vec![ctl_z_data.challenge; config.num_challenges],
            },
            &mut Challenger::new(),
            &mut timing,
        )?;

        timing.print();
        Ok(())
    }

    fn init_logger() {
        let _ = try_init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "debug"));
    }
}