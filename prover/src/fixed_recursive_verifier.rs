use core::mem::{self, MaybeUninit};
use std::collections::BTreeMap;
use std::ops::Range;

use hashbrown::HashMap;
use itertools::{zip_eq, Itertools};
use plonky2::field::extension::Extendable;
use plonky2::fri::FriParams;
use plonky2::gates::constant::ConstantGate;
use plonky2::gates::noop::NoopGate;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::challenger::RecursiveChallenger;
use plonky2::iop::target::{BoolTarget, Target};
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_builder::CircuitBuilder;
use plonky2::plonk::circuit_data::{
    CircuitConfig, CircuitData, CommonCircuitData, VerifierCircuitTarget,
};
use plonky2::plonk::config::{AlgebraicHasher, GenericConfig};
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::recursion::cyclic_recursion::check_cyclic_proof_verifier_data;
use plonky2::recursion::dummy_circuit::cyclic_base_proof;
use plonky2::util::serialization::{
    Buffer, GateSerializer, IoResult, Read, WitnessGeneratorSerializer, Write,
};
use plonky2::util::timing::TimingTree;
use plonky2_util::log2_ceil;

use crate::all_stark::{all_cross_table_lookups, AllStark, Table, NUM_TABLES};
use crate::config::StarkConfig;
use crate::cpu::kernel::assembler::Kernel;
use crate::cross_table_lookup::{
    get_grand_product_challenge_set_target, verify_cross_table_lookups_circuit, CrossTableLookup,
    GrandProductChallengeSet,
};
use crate::generation::state::{
    AssumptionReceipt, AssumptionReceipts, CompositeReceipt, InnerReceipt, Receipt, ReceiptClaim,
};
use crate::get_challenges::observe_public_values_target;
use crate::proof::{MemRootsTarget, PublicValues, PublicValuesTarget, StarkProofWithMetadata};
use crate::prover::{prove_with_output_and_assumptions, prove_with_outputs};
use crate::recursive_verifier::{
    add_common_recursion_gates, add_virtual_public_values, recursive_stark_circuit,
    set_public_value_targets, PlonkWrapperCircuit, PublicInputs, StarkWrapperCircuit,
};
use crate::stark::Stark;
use crate::util::u32_array_to_u8_vec;
use crate::verifier::verify_proof;
//use crate::util::h256_limbs;

/// The recursion threshold. We end a chain of recursive proofs once we reach this size.
const THRESHOLD_DEGREE_BITS: usize = 13;

pub const RANGE_TABLES: [&str; 12] = [
    "ARITHMETIC",
    "CPU",
    "POSEIDON",
    "POSEIDON_SPONGE",
    "KECCAK",
    "KECCAK_SPONGE",
    "SHA_EXTEND",
    "SHA_EXTEND_SPONGE",
    "SHA_COMPRESS",
    "SHA_COMPRESS_SPONGE",
    "LOGIC",
    "MEMORY",
];

/// Contains all recursive circuits used in the system.
///
/// For each STARK and each initial `degree_bits`, this contains a chain of
/// recursive circuits for shrinking that STARK from `degree_bits` to a constant
/// `THRESHOLD_DEGREE_BITS`. It also contains a special root circuit
/// for combining each STARK's shrunk wrapper proof into a single proof.
#[derive(Eq, PartialEq, Debug)]
pub struct AllRecursiveCircuits<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    C::Hasher: AlgebraicHasher<F>,
{
    /// The ZKVM root circuit, which aggregates the (shrunk) per-table recursive proofs.
    pub root: RootCircuitData<F, C, D>,
    pub aggregation: AggregationCircuitData<F, C, D>,
    /// The block circuit, which verifies an aggregation root proof and a previous block proof.
    pub block: BlockCircuitData<F, C, D>,
    /// Holds chains of circuits for each table and for each initial `degree_bits`.
    by_table: [RecursiveCircuitsForTable<F, C, D>; NUM_TABLES],
}

/// Data for the ZKVM root circuit, which is used to combine each STARK's shrunk wrapper proof
/// into a single proof.
#[derive(Eq, PartialEq, Debug)]
pub struct RootCircuitData<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub circuit: CircuitData<F, C, D>,
    proof_with_pis: [ProofWithPublicInputsTarget<D>; NUM_TABLES],
    /// For each table, various inner circuits may be used depending on the initial table size.
    /// This target holds the index of the circuit (within `final_circuits()`) that was used.
    index_verifier_data: [Target; NUM_TABLES],
    /// Public inputs containing public values.
    public_values: PublicValuesTarget,
    /// Public inputs used for cyclic verification. These aren't actually used for ZKVM root
    /// proofs; the circuit has them just to match the structure of aggregation proofs.
    cyclic_vk: VerifierCircuitTarget,
}

impl<F, C, const D: usize> RootCircuitData<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub fn to_buffer(
        &self,
        buffer: &mut Vec<u8>,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<()> {
        buffer.write_circuit_data(&self.circuit, gate_serializer, generator_serializer)?;
        for proof in &self.proof_with_pis {
            buffer.write_target_proof_with_public_inputs(proof)?;
        }
        for index in self.index_verifier_data {
            buffer.write_target(index)?;
        }
        self.public_values.to_buffer(buffer)?;
        buffer.write_target_verifier_circuit(&self.cyclic_vk)?;
        Ok(())
    }

    pub fn from_buffer(
        buffer: &mut Buffer,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let circuit = buffer.read_circuit_data(gate_serializer, generator_serializer)?;
        let mut proof_with_pis = Vec::with_capacity(NUM_TABLES);
        for _ in 0..NUM_TABLES {
            proof_with_pis.push(buffer.read_target_proof_with_public_inputs()?);
        }
        let mut index_verifier_data = Vec::with_capacity(NUM_TABLES);
        for _ in 0..NUM_TABLES {
            index_verifier_data.push(buffer.read_target()?);
        }
        let public_values = PublicValuesTarget::from_buffer(buffer)?;
        let cyclic_vk = buffer.read_target_verifier_circuit()?;

        Ok(Self {
            circuit,
            proof_with_pis: proof_with_pis.try_into().unwrap(),
            index_verifier_data: index_verifier_data.try_into().unwrap(),
            public_values,
            cyclic_vk,
        })
    }
}

/// Data for the aggregation circuit, which is used to compress two proofs into one. Each inner
/// proof can be either an ZKVM root proof or another aggregation proof.
#[derive(Eq, PartialEq, Debug)]
pub struct AggregationCircuitData<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub circuit: CircuitData<F, C, D>,
    lhs: AggregationChildTarget<D>,
    rhs: AggregationChildTarget<D>,
    public_values: PublicValuesTarget,
    cyclic_vk: VerifierCircuitTarget,
}

impl<F, C, const D: usize> AggregationCircuitData<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub fn to_buffer(
        &self,
        buffer: &mut Vec<u8>,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<()> {
        buffer.write_circuit_data(&self.circuit, gate_serializer, generator_serializer)?;
        buffer.write_target_verifier_circuit(&self.cyclic_vk)?;
        self.public_values.to_buffer(buffer)?;
        self.lhs.to_buffer(buffer)?;
        self.rhs.to_buffer(buffer)?;
        Ok(())
    }

    pub fn from_buffer(
        buffer: &mut Buffer,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let circuit = buffer.read_circuit_data(gate_serializer, generator_serializer)?;
        let cyclic_vk = buffer.read_target_verifier_circuit()?;
        let public_values = PublicValuesTarget::from_buffer(buffer)?;
        let lhs = AggregationChildTarget::from_buffer(buffer)?;
        let rhs = AggregationChildTarget::from_buffer(buffer)?;
        Ok(Self {
            circuit,
            lhs,
            rhs,
            public_values,
            cyclic_vk,
        })
    }
}

#[derive(Eq, PartialEq, Debug)]
pub struct AggregationChildTarget<const D: usize> {
    is_agg: BoolTarget,
    agg_proof: ProofWithPublicInputsTarget<D>,
    evm_proof: ProofWithPublicInputsTarget<D>,
}

impl<const D: usize> AggregationChildTarget<D> {
    pub fn to_buffer(&self, buffer: &mut Vec<u8>) -> IoResult<()> {
        buffer.write_target_bool(self.is_agg)?;
        buffer.write_target_proof_with_public_inputs(&self.agg_proof)?;
        buffer.write_target_proof_with_public_inputs(&self.evm_proof)?;
        Ok(())
    }

    pub fn from_buffer(buffer: &mut Buffer) -> IoResult<Self> {
        let is_agg = buffer.read_target_bool()?;
        let agg_proof = buffer.read_target_proof_with_public_inputs()?;
        let evm_proof = buffer.read_target_proof_with_public_inputs()?;
        Ok(Self {
            is_agg,
            agg_proof,
            evm_proof,
        })
    }

    pub fn public_values<F: RichField + Extendable<D>>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
    ) -> PublicValuesTarget {
        let agg_pv = PublicValuesTarget::from_public_inputs(&self.agg_proof.public_inputs);
        let evm_pv = PublicValuesTarget::from_public_inputs(&self.evm_proof.public_inputs);
        PublicValuesTarget::select(builder, self.is_agg, agg_pv, evm_pv)
    }
}

#[derive(Eq, PartialEq, Debug)]
pub struct BlockCircuitData<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub circuit: CircuitData<F, C, D>,
    has_parent_block: BoolTarget,
    parent_block_proof: ProofWithPublicInputsTarget<D>,
    agg_root_proof: ProofWithPublicInputsTarget<D>,
    public_values: PublicValuesTarget,
    cyclic_vk: VerifierCircuitTarget,
}

impl<F, C, const D: usize> BlockCircuitData<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
{
    pub fn to_buffer(
        &self,
        buffer: &mut Vec<u8>,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<()> {
        buffer.write_circuit_data(&self.circuit, gate_serializer, generator_serializer)?;
        buffer.write_target_bool(self.has_parent_block)?;
        buffer.write_target_proof_with_public_inputs(&self.parent_block_proof)?;
        buffer.write_target_proof_with_public_inputs(&self.agg_root_proof)?;
        self.public_values.to_buffer(buffer)?;
        buffer.write_target_verifier_circuit(&self.cyclic_vk)?;
        Ok(())
    }

    pub fn from_buffer(
        buffer: &mut Buffer,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let circuit = buffer.read_circuit_data(gate_serializer, generator_serializer)?;
        let has_parent_block = buffer.read_target_bool()?;
        let parent_block_proof = buffer.read_target_proof_with_public_inputs()?;
        let agg_root_proof = buffer.read_target_proof_with_public_inputs()?;
        let public_values = PublicValuesTarget::from_buffer(buffer)?;
        let cyclic_vk = buffer.read_target_verifier_circuit()?;
        Ok(Self {
            circuit,
            has_parent_block,
            parent_block_proof,
            agg_root_proof,
            public_values,
            cyclic_vk,
        })
    }
}

impl<F, C, const D: usize> AllRecursiveCircuits<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F> + 'static,
    C::Hasher: AlgebraicHasher<F>,
{
    pub fn to_bytes(
        &self,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Vec<u8>> {
        // TODO: would be better to initialize it dynamically based on the supported max degree.
        let mut buffer = Vec::with_capacity(1 << 34);
        self.root
            .to_buffer(&mut buffer, gate_serializer, generator_serializer)?;
        self.aggregation
            .to_buffer(&mut buffer, gate_serializer, generator_serializer)?;
        self.block
            .to_buffer(&mut buffer, gate_serializer, generator_serializer)?;
        for table in &self.by_table {
            table.to_buffer(&mut buffer, gate_serializer, generator_serializer)?;
        }
        Ok(buffer)
    }

    #[allow(clippy::missing_transmute_annotations)]
    pub fn from_bytes(
        bytes: &[u8],
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let mut buffer = Buffer::new(bytes);
        let root =
            RootCircuitData::from_buffer(&mut buffer, gate_serializer, generator_serializer)?;
        let aggregation = AggregationCircuitData::from_buffer(
            &mut buffer,
            gate_serializer,
            generator_serializer,
        )?;
        let block =
            BlockCircuitData::from_buffer(&mut buffer, gate_serializer, generator_serializer)?;

        // Tricky use of MaybeUninit to remove the need for implementing Debug
        // for all underlying types, necessary to convert a by_table Vec to an array.
        let by_table = {
            let mut by_table: [MaybeUninit<RecursiveCircuitsForTable<F, C, D>>; NUM_TABLES] =
                unsafe { MaybeUninit::uninit().assume_init() };
            for table in &mut by_table[..] {
                let value = RecursiveCircuitsForTable::from_buffer(
                    &mut buffer,
                    gate_serializer,
                    generator_serializer,
                )?;
                *table = MaybeUninit::new(value);
            }
            unsafe {
                #[allow(clippy::missing_transmute_annotations)]
                mem::transmute::<_, [RecursiveCircuitsForTable<F, C, D>; NUM_TABLES]>(by_table)
            }
        };

        Ok(Self {
            root,
            aggregation,
            block,
            by_table,
        })
    }

    /// Preprocess all recursive circuits used by the system.
    pub fn new(
        all_stark: &AllStark<F, D>,
        degree_bits_ranges: &[Range<usize>; NUM_TABLES],
        stark_config: &StarkConfig,
    ) -> Self {
        let arithmetic = RecursiveCircuitsForTable::new(
            Table::Arithmetic,
            &all_stark.arithmetic_stark,
            degree_bits_ranges[Table::Arithmetic as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let cpu = RecursiveCircuitsForTable::new(
            Table::Cpu,
            &all_stark.cpu_stark,
            degree_bits_ranges[Table::Cpu as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let poseidon = RecursiveCircuitsForTable::new(
            Table::Poseidon,
            &all_stark.poseidon_stark,
            degree_bits_ranges[Table::Poseidon as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let poseidon_sponge = RecursiveCircuitsForTable::new(
            Table::PoseidonSponge,
            &all_stark.poseidon_sponge_stark,
            degree_bits_ranges[Table::PoseidonSponge as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let keccak = RecursiveCircuitsForTable::new(
            Table::Keccak,
            &all_stark.keccak_stark,
            degree_bits_ranges[Table::Keccak as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let keccak_sponge = RecursiveCircuitsForTable::new(
            Table::KeccakSponge,
            &all_stark.keccak_sponge_stark,
            degree_bits_ranges[Table::KeccakSponge as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let sha_extend = RecursiveCircuitsForTable::new(
            Table::ShaExtend,
            &all_stark.sha_extend_stark,
            degree_bits_ranges[Table::ShaExtend as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let sha_extend_sponge = RecursiveCircuitsForTable::new(
            Table::ShaExtendSponge,
            &all_stark.sha_extend_sponge_stark,
            degree_bits_ranges[Table::ShaExtendSponge as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let sha_compress = RecursiveCircuitsForTable::new(
            Table::ShaCompress,
            &all_stark.sha_compress_stark,
            degree_bits_ranges[Table::ShaCompress as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let sha_compress_sponge = RecursiveCircuitsForTable::new(
            Table::ShaCompressSponge,
            &all_stark.sha_compress_sponge_stark,
            degree_bits_ranges[Table::ShaCompressSponge as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let logic = RecursiveCircuitsForTable::new(
            Table::Logic,
            &all_stark.logic_stark,
            degree_bits_ranges[Table::Logic as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );
        let memory = RecursiveCircuitsForTable::new(
            Table::Memory,
            &all_stark.memory_stark,
            degree_bits_ranges[Table::Memory as usize].clone(),
            &all_stark.cross_table_lookups,
            stark_config,
        );

        let by_table = [
            arithmetic,
            cpu,
            poseidon,
            poseidon_sponge,
            keccak,
            keccak_sponge,
            sha_extend,
            sha_extend_sponge,
            sha_compress,
            sha_compress_sponge,
            logic,
            memory,
        ];
        let root = Self::create_root_circuit(&by_table, stark_config);
        let aggregation = Self::create_aggregation_circuit(&root);
        let block = Self::create_block_circuit(&aggregation);
        Self {
            root,
            aggregation,
            block,
            by_table,
        }
    }

    fn create_root_circuit(
        by_table: &[RecursiveCircuitsForTable<F, C, D>; NUM_TABLES],
        stark_config: &StarkConfig,
    ) -> RootCircuitData<F, C, D> {
        let inner_common_data: [_; NUM_TABLES] =
            core::array::from_fn(|i| &by_table[i].final_circuits()[0].common);

        let mut builder = CircuitBuilder::new(CircuitConfig::standard_recursion_config());

        let public_values = add_virtual_public_values(&mut builder);

        let recursive_proofs =
            core::array::from_fn(|i| builder.add_virtual_proof_with_pis(inner_common_data[i]));
        let pis: [_; NUM_TABLES] = core::array::from_fn(|i| {
            PublicInputs::<Target, <C::Hasher as AlgebraicHasher<F>>::AlgebraicPermutation>::from_vec(
                &recursive_proofs[i].public_inputs,
                stark_config,
            )
        });
        let index_verifier_data = core::array::from_fn(|_i| builder.add_virtual_target());

        let mut challenger = RecursiveChallenger::<F, C::Hasher, D>::new(&mut builder);
        for pi in &pis {
            for h in &pi.trace_cap {
                challenger.observe_elements(h);
            }
        }

        observe_public_values_target::<F, C, D>(&mut challenger, &public_values);

        let ctl_challenges = get_grand_product_challenge_set_target(
            &mut builder,
            &mut challenger,
            stark_config.num_challenges,
        );
        // Check that the correct CTL challenges are used in every proof.
        for pi in &pis {
            for i in 0..stark_config.num_challenges {
                builder.connect(
                    ctl_challenges.challenges[i].beta,
                    pi.ctl_challenges.challenges[i].beta,
                );
                builder.connect(
                    ctl_challenges.challenges[i].gamma,
                    pi.ctl_challenges.challenges[i].gamma,
                );
            }
        }

        let state = challenger.compact(&mut builder);
        for (&before, &s) in zip_eq(state.as_ref(), pis[0].challenger_state_before.as_ref()) {
            builder.connect(before, s);
        }
        // Check that the challenger state is consistent between proofs.
        for i in 1..NUM_TABLES {
            for (&before, &after) in zip_eq(
                pis[i].challenger_state_before.as_ref(),
                pis[i - 1].challenger_state_after.as_ref(),
            ) {
                builder.connect(before, after);
            }
        }

        // Verify the CTL checks.
        verify_cross_table_lookups_circuit::<F, D>(
            &mut builder,
            all_cross_table_lookups(),
            pis.map(|p| p.ctl_zs_first),
            stark_config,
        );

        for (i, table_circuits) in by_table.iter().enumerate() {
            let final_circuits = table_circuits.final_circuits();
            for final_circuit in &final_circuits {
                assert_eq!(
                    &final_circuit.common, inner_common_data[i],
                    "common_data mismatch"
                );
            }
            let mut possible_vks = final_circuits
                .into_iter()
                .map(|c| builder.constant_verifier_data(&c.verifier_only))
                .collect_vec();
            // random_access_verifier_data expects a vector whose length is a power of two.
            // To satisfy this, we will just add some duplicates of the first VK.
            while !possible_vks.len().is_power_of_two() {
                possible_vks.push(possible_vks[0].clone());
            }
            let inner_verifier_data =
                builder.random_access_verifier_data(index_verifier_data[i], possible_vks);

            builder.verify_proof::<C>(
                &recursive_proofs[i],
                &inner_verifier_data,
                inner_common_data[i],
            );
        }

        // We want ZKVM root proofs to have the exact same structure as aggregation proofs, so we add
        // public inputs for cyclic verification, even though they'll be ignored.
        let cyclic_vk = builder.add_verifier_data_public_inputs();

        builder.add_gate(
            ConstantGate::new(inner_common_data[0].config.num_constants),
            vec![],
        );

        RootCircuitData {
            circuit: builder.build::<C>(),
            proof_with_pis: recursive_proofs,
            index_verifier_data,
            public_values,
            cyclic_vk,
        }
    }

    fn create_aggregation_circuit(
        root: &RootCircuitData<F, C, D>,
    ) -> AggregationCircuitData<F, C, D> {
        let mut builder = CircuitBuilder::<F, D>::new(root.circuit.common.config.clone());
        let public_values = add_virtual_public_values(&mut builder);
        let cyclic_vk = builder.add_verifier_data_public_inputs();
        let lhs = Self::add_agg_child(&mut builder, root);
        let rhs = Self::add_agg_child(&mut builder, root);

        let lhs_public_values = lhs.public_values(&mut builder);
        let rhs_public_values = rhs.public_values(&mut builder);
        // Connect aggregation `trie_roots_before` with lhs `trie_roots_before`.
        MemRootsTarget::connect(
            &mut builder,
            public_values.roots_before,
            lhs_public_values.roots_before,
        );
        // Connect aggregation `trie_roots_after` with rhs `trie_roots_after`.
        MemRootsTarget::connect(
            &mut builder,
            public_values.roots_after,
            rhs_public_values.roots_after,
        );
        // Connect lhs `trie_roots_after` with rhs `trie_roots_before`.
        MemRootsTarget::connect(
            &mut builder,
            lhs_public_values.roots_after,
            rhs_public_values.roots_before,
        );

        // Connect agg `userdata` with lhs `userdata`.
        for (limb0, limb1) in public_values
            .userdata
            .iter()
            .zip_eq(&lhs_public_values.userdata)
        {
            builder.connect(*limb0, *limb1);
        }

        // Connect agg `userdata` with rhs `userdata`.
        for (limb0, limb1) in public_values
            .userdata
            .iter()
            .zip_eq(&rhs_public_values.userdata)
        {
            builder.connect(*limb0, *limb1);
        }

        // Pad to match the root circuit's degree.
        while log2_ceil(builder.num_gates()) < root.circuit.common.degree_bits() {
            builder.add_gate(NoopGate, vec![]);
        }

        let circuit = builder.build::<C>();
        AggregationCircuitData {
            circuit,
            lhs,
            rhs,
            public_values,
            cyclic_vk,
        }
    }

    fn add_agg_child(
        builder: &mut CircuitBuilder<F, D>,
        root: &RootCircuitData<F, C, D>,
    ) -> AggregationChildTarget<D> {
        let common = &root.circuit.common;
        let root_vk = builder.constant_verifier_data(&root.circuit.verifier_only);
        let is_agg = builder.add_virtual_bool_target_safe();
        let agg_proof = builder.add_virtual_proof_with_pis(common);
        let evm_proof = builder.add_virtual_proof_with_pis(common);
        builder
            .conditionally_verify_cyclic_proof::<C>(
                is_agg, &agg_proof, &evm_proof, &root_vk, common,
            )
            .expect("Failed to build cyclic recursion circuit");
        AggregationChildTarget {
            is_agg,
            agg_proof,
            evm_proof,
        }
    }

    fn create_block_circuit(agg: &AggregationCircuitData<F, C, D>) -> BlockCircuitData<F, C, D> {
        // The block circuit is similar to the agg circuit; both verify two inner proofs.
        // We need to adjust a few things, but it's easier than making a new CommonCircuitData.
        let expected_common_data = CommonCircuitData {
            fri_params: FriParams {
                degree_bits: 14,
                ..agg.circuit.common.fri_params.clone()
            },
            ..agg.circuit.common.clone()
        };

        let mut builder = CircuitBuilder::<F, D>::new(CircuitConfig::standard_recursion_config());
        let public_values = add_virtual_public_values(&mut builder);
        let has_parent_block = builder.add_virtual_bool_target_safe();
        let parent_block_proof = builder.add_virtual_proof_with_pis(&expected_common_data);
        let agg_root_proof = builder.add_virtual_proof_with_pis(&agg.circuit.common);

        let parent_pv = PublicValuesTarget::from_public_inputs(&parent_block_proof.public_inputs);
        let agg_pv = PublicValuesTarget::from_public_inputs(&agg_root_proof.public_inputs);

        // Connect block `trie_roots_before` with parent_pv `trie_roots_before`.
        MemRootsTarget::connect(
            &mut builder,
            public_values.roots_before,
            parent_pv.roots_before,
        );
        // Connect the rest of block `public_values` with agg_pv.
        MemRootsTarget::connect(&mut builder, public_values.roots_after, agg_pv.roots_after);

        // Make connections between block proofs, and check initial and final block values.
        Self::connect_block_proof(&mut builder, has_parent_block, &parent_pv, &agg_pv);

        for (&limb0, &limb1) in parent_pv.userdata.iter().zip_eq(&agg_pv.userdata) {
            builder.connect(limb0, limb1);
        }

        let cyclic_vk = builder.add_verifier_data_public_inputs();
        builder
            .conditionally_verify_cyclic_proof_or_dummy::<C>(
                has_parent_block,
                &parent_block_proof,
                &expected_common_data,
            )
            .expect("Failed to build cyclic recursion circuit");

        let agg_verifier_data = builder.constant_verifier_data(&agg.circuit.verifier_only);
        builder.verify_proof::<C>(&agg_root_proof, &agg_verifier_data, &agg.circuit.common);

        let circuit = builder.build::<C>();
        BlockCircuitData {
            circuit,
            has_parent_block,
            parent_block_proof,
            agg_root_proof,
            public_values,
            cyclic_vk,
        }
    }

    fn connect_block_proof(
        builder: &mut CircuitBuilder<F, D>,
        _has_parent_block: BoolTarget,
        lhs: &PublicValuesTarget,
        rhs: &PublicValuesTarget,
    ) {
        // Between blocks, we only connect state tries and userdata.
        for (&limb0, limb1) in lhs.roots_after.root.iter().zip(rhs.roots_before.root) {
            builder.connect(limb0, limb1);
        }
    }

    /// Create a proof for each STARK, then combine them, eventually culminating in a root proof.
    pub fn prove_root(
        &self,
        all_stark: &AllStark<F, D>,
        kernel: &Kernel,
        config: &StarkConfig,
        timing: &mut TimingTree,
    ) -> anyhow::Result<Receipt<F, C, D>> {
        let (all_proof, output) = prove_with_outputs::<F, C, D>(all_stark, kernel, config, timing)?;
        verify_proof(all_stark, all_proof.clone(), config).unwrap();
        let mut root_inputs = PartialWitness::new();
        for table in 0..NUM_TABLES {
            let stark_proof = &all_proof.stark_proofs[table];
            let original_degree_bits = stark_proof.proof.recover_degree_bits(config);
            let table_circuits = &self.by_table[table];
            let shrunk_proof = table_circuits
                .by_stark_size
                .get(&original_degree_bits)
                .ok_or_else(|| {
                    anyhow::Error::msg(format!(
                        "Missing preprocessed circuits for {:?} table with size {}. To set it, run: export {}=\"{}..{}\" ",
                        Table::all()[table],
                        original_degree_bits,
                        RANGE_TABLES[table],
                        original_degree_bits,
                        original_degree_bits + 1,
                    ))
                })?
                .shrink(stark_proof, &all_proof.ctl_challenges)?;
            let index_verifier_data = table_circuits
                .by_stark_size
                .keys()
                .position(|&size| size == original_degree_bits)
                .unwrap();
            root_inputs.set_target(
                self.root.index_verifier_data[table],
                F::from_canonical_usize(index_verifier_data),
            );
            root_inputs.set_proof_with_pis_target(&self.root.proof_with_pis[table], &shrunk_proof);
        }

        root_inputs.set_verifier_data_target(
            &self.root.cyclic_vk,
            &self.aggregation.circuit.verifier_only,
        );

        set_public_value_targets(
            &mut root_inputs,
            &self.root.public_values,
            &all_proof.public_values,
        )
        .map_err(|_| {
            anyhow::Error::msg("Invalid conversion when setting public values targets.")
        })?;

        let root_proof = self.root.circuit.prove(root_inputs)?;

        Ok(Receipt::Segments(InnerReceipt {
            proof: root_proof,
            values: all_proof.public_values.clone(),
            claim: ReceiptClaim {
                elf_id: u32_array_to_u8_vec(&all_proof.public_values.roots_before.root),
                commit: output.output.clone(),
            },
        }))
    }

    pub fn prove_root_with_assumption(
        &self,
        all_stark: &AllStark<F, D>,
        kernel: &Kernel,
        config: &StarkConfig,
        timing: &mut TimingTree,
        assumptions: AssumptionReceipts<F, C, D>,
    ) -> anyhow::Result<Receipt<F, C, D>> {
        if assumptions.is_empty() {
            return self.prove_root(all_stark, kernel, config, timing);
        }
        let (all_proof, output, assumption_used) = prove_with_output_and_assumptions::<F, C, D>(
            all_stark,
            kernel,
            config,
            timing,
            assumptions,
        )?;
        verify_proof(all_stark, all_proof.clone(), config).unwrap();
        let mut root_inputs = PartialWitness::new();

        for table in 0..NUM_TABLES {
            let stark_proof = &all_proof.stark_proofs[table];
            let original_degree_bits = stark_proof.proof.recover_degree_bits(config);
            let table_circuits = &self.by_table[table];
            let shrunk_proof = table_circuits
                .by_stark_size
                .get(&original_degree_bits)
                .ok_or_else(|| {
                    anyhow::Error::msg(format!(
                        "Missing preprocessed circuits for {:?} table with size {}. To set it, run: export {}=\"{}..{}\" ",
                        Table::all()[table],
                        original_degree_bits,
                        RANGE_TABLES[table],
                        original_degree_bits,
                        original_degree_bits + 1,
                    ))
                })?
                .shrink(stark_proof, &all_proof.ctl_challenges)?;
            let index_verifier_data = table_circuits
                .by_stark_size
                .keys()
                .position(|&size| size == original_degree_bits)
                .unwrap();
            root_inputs.set_target(
                self.root.index_verifier_data[table],
                F::from_canonical_usize(index_verifier_data),
            );
            root_inputs.set_proof_with_pis_target(&self.root.proof_with_pis[table], &shrunk_proof);
        }

        root_inputs.set_verifier_data_target(
            &self.root.cyclic_vk,
            &self.aggregation.circuit.verifier_only,
        );

        set_public_value_targets(
            &mut root_inputs,
            &self.root.public_values,
            &all_proof.public_values,
        )
        .map_err(|_| {
            anyhow::Error::msg("Invalid conversion when setting public values targets.")
        })?;

        let root_proof = self.root.circuit.prove(root_inputs)?;

        let program_receipt = InnerReceipt {
            proof: root_proof,
            values: all_proof.public_values.clone(),
            claim: ReceiptClaim {
                elf_id: u32_array_to_u8_vec(&all_proof.public_values.roots_before.root),
                commit: output.output.clone(),
            },
        };
        Ok(Receipt::Composite(CompositeReceipt {
            program_receipt,
            assumption_used,
        }))
    }

    pub fn verify_root(&self, agg_receipt: Receipt<F, C, D>) -> anyhow::Result<()> {
        self.root.circuit.verify(agg_receipt.proof())
    }

    pub fn prove_aggregation(
        &self,
        lhs_is_agg: bool,
        lhs_receipt: &Receipt<F, C, D>,
        rhs_is_agg: bool,
        rhs_receipt: &Receipt<F, C, D>,
    ) -> anyhow::Result<Receipt<F, C, D>> {
        let mut agg_inputs = PartialWitness::new();

        let public_values = PublicValues {
            roots_before: lhs_receipt.values().roots_before,
            roots_after: rhs_receipt.values().roots_after,
            userdata: rhs_receipt.values().userdata,
        };

        agg_inputs.set_bool_target(self.aggregation.lhs.is_agg, lhs_is_agg);
        agg_inputs.set_proof_with_pis_target(&self.aggregation.lhs.agg_proof, &lhs_receipt.proof());
        agg_inputs.set_proof_with_pis_target(&self.aggregation.lhs.evm_proof, &lhs_receipt.proof());

        agg_inputs.set_bool_target(self.aggregation.rhs.is_agg, rhs_is_agg);
        agg_inputs.set_proof_with_pis_target(&self.aggregation.rhs.agg_proof, &rhs_receipt.proof());
        agg_inputs.set_proof_with_pis_target(&self.aggregation.rhs.evm_proof, &rhs_receipt.proof());

        agg_inputs.set_verifier_data_target(
            &self.aggregation.cyclic_vk,
            &self.aggregation.circuit.verifier_only,
        );

        set_public_value_targets(
            &mut agg_inputs,
            &self.aggregation.public_values,
            &public_values,
        )
        .map_err(|_| {
            anyhow::Error::msg("Invalid conversion when setting public values targets.")
        })?;

        let aggregation_proof = self.aggregation.circuit.prove(agg_inputs)?;
        let inner = InnerReceipt {
            proof: aggregation_proof,
            values: public_values,
            claim: ReceiptClaim {
                elf_id: lhs_receipt.claim().clone().elf_id,
                commit: rhs_receipt.claim().clone().commit,
            },
        };

        let assumptions = lhs_receipt.assumptions();
        for assumption in rhs_receipt.assumptions().borrow().iter() {
            assumptions.borrow_mut().insert(0, assumption.clone());
        }

        if assumptions.borrow().is_empty() {
            Ok(Receipt::Segments(inner))
        } else {
            Ok(Receipt::Composite(CompositeReceipt {
                program_receipt: inner,
                assumption_used: assumptions,
            }))
        }
    }

    pub fn verify_aggregation(&self, receipt: &Receipt<F, C, D>) -> anyhow::Result<()> {
        self.aggregation.circuit.verify(receipt.proof())?;
        check_cyclic_proof_verifier_data(
            &receipt.proof(),
            &self.aggregation.circuit.verifier_only,
            &self.aggregation.circuit.common,
        )
    }

    pub fn prove_block(
        &self,
        opt_parent_block_receipt: Option<&Receipt<F, C, D>>,
        agg_root_receipt: &Receipt<F, C, D>,
    ) -> anyhow::Result<Receipt<F, C, D>> {
        let mut block_inputs = PartialWitness::new();

        block_inputs.set_bool_target(
            self.block.has_parent_block,
            opt_parent_block_receipt.is_some(),
        );
        if let Some(parent_block_receipt) = opt_parent_block_receipt {
            block_inputs.set_proof_with_pis_target(
                &self.block.parent_block_proof,
                &parent_block_receipt.proof(),
            );
        } else {
            // Initialize `state_root_after`.
            let mut nonzero_pis = HashMap::new();
            let state_trie_root_before_keys = 0..8;
            for (key, &value) in
                state_trie_root_before_keys.zip_eq(&agg_root_receipt.values().roots_before.root)
            {
                nonzero_pis.insert(key, F::from_canonical_u32(value));
            }

            let state_trie_root_after_keys = 8..16;
            for (key, &value) in
                state_trie_root_after_keys.zip_eq(&agg_root_receipt.values().roots_before.root)
            {
                nonzero_pis.insert(key, F::from_canonical_u32(value));
            }

            let userdata_keys = 16..16 + agg_root_receipt.values().userdata.len();
            for (key, &value) in userdata_keys.zip_eq(&agg_root_receipt.values().userdata) {
                nonzero_pis.insert(key, F::from_canonical_u8(value));
            }

            block_inputs.set_proof_with_pis_target(
                &self.block.parent_block_proof,
                &cyclic_base_proof(
                    &self.block.circuit.common,
                    &self.block.circuit.verifier_only,
                    nonzero_pis,
                ),
            );
        }

        block_inputs
            .set_proof_with_pis_target(&self.block.agg_root_proof, &agg_root_receipt.proof());

        block_inputs
            .set_verifier_data_target(&self.block.cyclic_vk, &self.block.circuit.verifier_only);

        set_public_value_targets(
            &mut block_inputs,
            &self.block.public_values,
            &agg_root_receipt.values(),
        )
        .map_err(|_| {
            anyhow::Error::msg("Invalid conversion when setting public values targets.")
        })?;

        let block_proof = self.block.circuit.prove(block_inputs)?;
        let inner = InnerReceipt {
            proof: block_proof,
            values: agg_root_receipt.values(),
            claim: agg_root_receipt.claim(),
        };
        match agg_root_receipt {
            Receipt::Segments(_receipt) => Ok(Receipt::Segments(inner)),
            Receipt::Composite(receipt) => Ok(Receipt::Composite(CompositeReceipt {
                program_receipt: inner,
                assumption_used: receipt.assumption_used.clone(),
            })),
        }
    }

    pub fn verify_block(&self, block_receipt: &Receipt<F, C, D>) -> anyhow::Result<()> {
        self.block.circuit.verify(block_receipt.proof())?;
        match block_receipt {
            Receipt::Segments(_receipt) => (),
            Receipt::Composite(receipt) => {
                for assumption in receipt.assumption_used.borrow_mut().iter_mut() {
                    let receipt = assumption.1.clone();
                    match receipt {
                        AssumptionReceipt::<F, C, D>::Proven(inner) => {
                            self.verify_root(Receipt::Segments(*inner))?;
                        }
                        AssumptionReceipt::Unresolved(assumpt) => {
                            log::error!("unresolved assumption: {:X?}", assumpt);
                        }
                    }
                }
            }
        };
        check_cyclic_proof_verifier_data(
            &block_receipt.proof(),
            &self.block.circuit.verifier_only,
            &self.block.circuit.common,
        )
    }
}

#[derive(Eq, PartialEq, Debug)]
pub struct RecursiveCircuitsForTable<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    C::Hasher: AlgebraicHasher<F>,
{
    /// A map from `log_2(height)` to a chain of shrinking recursion circuits starting at that
    /// height.
    by_stark_size: BTreeMap<usize, RecursiveCircuitsForTableSize<F, C, D>>,
}

impl<F, C, const D: usize> RecursiveCircuitsForTable<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    C::Hasher: AlgebraicHasher<F>,
{
    pub fn to_buffer(
        &self,
        buffer: &mut Vec<u8>,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<()> {
        buffer.write_usize(self.by_stark_size.len())?;
        for (&size, table) in &self.by_stark_size {
            buffer.write_usize(size)?;
            table.to_buffer(buffer, gate_serializer, generator_serializer)?;
        }
        Ok(())
    }

    pub fn from_buffer(
        buffer: &mut Buffer,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let length = buffer.read_usize()?;
        let mut by_stark_size = BTreeMap::new();
        for _ in 0..length {
            let key = buffer.read_usize()?;
            let table = RecursiveCircuitsForTableSize::from_buffer(
                buffer,
                gate_serializer,
                generator_serializer,
            )?;
            by_stark_size.insert(key, table);
        }
        Ok(Self { by_stark_size })
    }

    fn new<S: Stark<F, D>>(
        table: Table,
        stark: &S,
        degree_bits_range: Range<usize>,
        all_ctls: &[CrossTableLookup<F>],
        stark_config: &StarkConfig,
    ) -> Self {
        let by_stark_size = degree_bits_range
            .map(|degree_bits| {
                (
                    degree_bits,
                    RecursiveCircuitsForTableSize::new::<S>(
                        table,
                        stark,
                        degree_bits,
                        all_ctls,
                        stark_config,
                    ),
                )
            })
            .collect();
        Self { by_stark_size }
    }

    /// For each initial `degree_bits`, get the final circuit at the end of that shrinking chain.
    /// Each of these final circuits should have degree `THRESHOLD_DEGREE_BITS`.
    fn final_circuits(&self) -> Vec<&CircuitData<F, C, D>> {
        self.by_stark_size
            .values()
            .map(|chain| {
                chain
                    .shrinking_wrappers
                    .last()
                    .map(|wrapper| &wrapper.circuit)
                    .unwrap_or(&chain.initial_wrapper.circuit)
            })
            .collect()
    }
}

/// A chain of shrinking wrapper circuits, ending with a final circuit with `degree_bits`
/// `THRESHOLD_DEGREE_BITS`.
#[derive(Eq, PartialEq, Debug)]
struct RecursiveCircuitsForTableSize<F, C, const D: usize>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    C::Hasher: AlgebraicHasher<F>,
{
    initial_wrapper: StarkWrapperCircuit<F, C, D>,
    shrinking_wrappers: Vec<PlonkWrapperCircuit<F, C, D>>,
}

impl<F, C, const D: usize> RecursiveCircuitsForTableSize<F, C, D>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    C::Hasher: AlgebraicHasher<F>,
{
    pub fn to_buffer(
        &self,
        buffer: &mut Vec<u8>,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<()> {
        buffer.write_usize(self.shrinking_wrappers.len())?;
        if !self.shrinking_wrappers.is_empty() {
            buffer.write_common_circuit_data(
                &self.shrinking_wrappers[0].circuit.common,
                gate_serializer,
            )?;
        }
        for wrapper in &self.shrinking_wrappers {
            buffer.write_prover_only_circuit_data(
                &wrapper.circuit.prover_only,
                generator_serializer,
                &wrapper.circuit.common,
            )?;
            buffer.write_verifier_only_circuit_data(&wrapper.circuit.verifier_only)?;
            buffer.write_target_proof_with_public_inputs(&wrapper.proof_with_pis_target)?;
        }
        self.initial_wrapper
            .to_buffer(buffer, gate_serializer, generator_serializer)?;
        Ok(())
    }

    pub fn from_buffer(
        buffer: &mut Buffer,
        gate_serializer: &dyn GateSerializer<F, D>,
        generator_serializer: &dyn WitnessGeneratorSerializer<F, D>,
    ) -> IoResult<Self> {
        let length = buffer.read_usize()?;
        let mut shrinking_wrappers = Vec::with_capacity(length);
        if length != 0 {
            let common = buffer.read_common_circuit_data(gate_serializer)?;

            for _ in 0..length {
                let prover_only =
                    buffer.read_prover_only_circuit_data(generator_serializer, &common)?;
                let verifier_only = buffer.read_verifier_only_circuit_data()?;
                let proof_with_pis_target = buffer.read_target_proof_with_public_inputs()?;
                shrinking_wrappers.push(PlonkWrapperCircuit {
                    circuit: CircuitData {
                        common: common.clone(),
                        prover_only,
                        verifier_only,
                    },
                    proof_with_pis_target,
                })
            }
        };

        let initial_wrapper =
            StarkWrapperCircuit::from_buffer(buffer, gate_serializer, generator_serializer)?;

        Ok(Self {
            initial_wrapper,
            shrinking_wrappers,
        })
    }

    fn new<S: Stark<F, D>>(
        table: Table,
        stark: &S,
        degree_bits: usize,
        all_ctls: &[CrossTableLookup<F>],
        stark_config: &StarkConfig,
    ) -> Self {
        let initial_wrapper = recursive_stark_circuit(
            table,
            stark,
            degree_bits,
            all_ctls,
            stark_config,
            &shrinking_config(),
            THRESHOLD_DEGREE_BITS,
        );
        let mut shrinking_wrappers = vec![];

        // Shrinking recursion loop.
        loop {
            let last = shrinking_wrappers
                .last()
                .map(|wrapper: &PlonkWrapperCircuit<F, C, D>| &wrapper.circuit)
                .unwrap_or(&initial_wrapper.circuit);
            let last_degree_bits = last.common.degree_bits();
            assert!(last_degree_bits >= THRESHOLD_DEGREE_BITS);
            if last_degree_bits == THRESHOLD_DEGREE_BITS {
                break;
            }

            let mut builder = CircuitBuilder::new(shrinking_config());
            let proof_with_pis_target = builder.add_virtual_proof_with_pis(&last.common);
            let last_vk = builder.constant_verifier_data(&last.verifier_only);
            builder.verify_proof::<C>(&proof_with_pis_target, &last_vk, &last.common);
            builder.register_public_inputs(&proof_with_pis_target.public_inputs); // carry PIs forward
            add_common_recursion_gates(&mut builder);
            let circuit = builder.build::<C>();

            assert!(
                circuit.common.degree_bits() < last_degree_bits,
                "Couldn't shrink to expected recursion threshold of 2^{}; stalled at 2^{}",
                THRESHOLD_DEGREE_BITS,
                circuit.common.degree_bits()
            );
            shrinking_wrappers.push(PlonkWrapperCircuit {
                circuit,
                proof_with_pis_target,
            });
        }

        Self {
            initial_wrapper,
            shrinking_wrappers,
        }
    }

    fn shrink(
        &self,
        stark_proof_with_metadata: &StarkProofWithMetadata<F, C, D>,
        ctl_challenges: &GrandProductChallengeSet<F>,
    ) -> anyhow::Result<ProofWithPublicInputs<F, C, D>> {
        let mut proof = self
            .initial_wrapper
            .prove(stark_proof_with_metadata, ctl_challenges)?;
        for wrapper_circuit in &self.shrinking_wrappers {
            proof = wrapper_circuit.prove(&proof)?;
        }
        Ok(proof)
    }
}

/// Our usual recursion threshold is 2^12 gates, but for these shrinking circuits, we use a few more
/// gates for a constant inner VK and for public inputs. This pushes us over the threshold to 2^13.
/// As long as we're at 2^13 gates, we might as well use a narrower witness.
fn shrinking_config() -> CircuitConfig {
    CircuitConfig {
        num_routed_wires: 40,
        ..CircuitConfig::standard_recursion_config()
    }
}
