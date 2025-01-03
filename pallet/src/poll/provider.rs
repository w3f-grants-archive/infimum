use sp_std::vec;
use sp_runtime::traits::SaturatedConversion;
use ark_bn254::{Fr};
use ark_ff::{PrimeField, BigInteger};
use crate::hash::{Poseidon, PoseidonHasher};
use crate::poll::{
    AmortizedIncrementalMerkleTree, 
    BlockNumber,
    Commitment,
    Coordinator,
    HashBytes,
    MerkleTreeError,
    OutcomeIndex,
    PollOutcome,
    Poll, 
    PublicKey,
    PollInteractionData,
    VerifyKey,
    zeroes::EMPTY_BALLOT_ROOTS
};

pub trait PollProvider<T: crate::Config>: Sized
{
    fn verify_outcome(
        self,
        oucome: Option<PollOutcome>
    ) -> Option<OutcomeIndex>;

    fn prepare_public_inputs(
        self,
        coordinator: Coordinator,
        new_commitment: HashBytes
    ) -> Option<(VerifyKey, vec::Vec<Fr>, Commitment)>;

    fn register_participant(
        self, 
        public_key: PublicKey, 
        timestamp: u64
    ) -> Result<(u32, Self), MerkleTreeError>;

    fn consume_interaction(
        self,
        public_key: PublicKey,
        data: PollInteractionData
    ) -> Result<(u32, Self), MerkleTreeError>;

    fn merge_registrations(self) -> Result<Self, MerkleTreeError>;

    fn merge_interactions(self) -> Result<Self, MerkleTreeError>;
    
    fn registration_limit_reached(&self) -> bool;

    fn interaction_limit_reached(&self) -> bool;

    fn is_voting_period(&self) -> bool;

    fn is_registration_period(&self) -> bool;

    fn get_voting_period_end(&self) -> BlockNumber;

    fn is_over(&self) -> bool;

    fn is_fulfilled(&self) -> bool;

    fn is_merged(&self) -> bool;

    fn is_proven(&self) -> bool;

    fn is_nullified(&self) -> bool;

    fn nullify(self) -> Self;
}

impl<T: crate::Config> PollProvider<T> for Poll<T>
{
    fn verify_outcome(
        self,
        outcome: Option<PollOutcome>
    ) -> Option<OutcomeIndex>
    {
        // Ensure that all of the expected proofs have been successfully verified.
        if !self.is_proven() { return None; }

        let Some(outcome) = outcome else { return None; };
        let Some(mut hasher) = Poseidon::<Fr>::new_circom(2).ok() else { return None; };
        
        let mut outcome_index: OutcomeIndex = 0;
        let mut max_tally_result = 0;

        // Verify the tally result for each individual vote option.
        for option_index in 0..self.config.vote_options.len()
        {
            let Some(tally_result) = outcome.tally_results.get(option_index) else { return None; };
            let Some(tally_path) = outcome.tally_result_proofs.get(option_index) else { return None; };
            let mut tally_result_bytes = [0u8; 32];
            tally_result_bytes[28..].copy_from_slice(&tally_result.to_be_bytes());

            let Some(root) = compute_merkle_root_from_path(
                self.config.vote_option_tree_depth,
                option_index as u32,
                tally_result_bytes,
                tally_path.clone()
            ) else { return None; };

            let mut inputs: vec::Vec<Fr> = vec::Vec::<Fr>::new();
            inputs.push(Fr::from_be_bytes_mod_order(&root));
            inputs.push(Fr::from_be_bytes_mod_order(&outcome.tally_result_salt));
            let Some(hash) = hasher.hash(&inputs).ok() else { return None; };

            let mut inputs: vec::Vec<Fr> = vec::Vec::<Fr>::new();
            inputs.push(Fr::from_be_bytes_mod_order(&hash.into_bigint().to_bytes_be()));
            inputs.push(Fr::from_be_bytes_mod_order(&outcome.spent_votes_hash));
            let Some(hash) = hasher.hash(&inputs).ok() else { return None; };

            if hash.into_bigint().to_bytes_be() != self.state.commitment.tally.1 { return None; }

            // Track the vote option with the largest tally.
            if *tally_result > max_tally_result
            {
                outcome_index = option_index as OutcomeIndex;
                max_tally_result = *tally_result;
            }
        }

        // Verify the total number of votes cast.
        let mut inputs: vec::Vec<Fr> = vec::Vec::<Fr>::new();
        inputs.push(Fr::from_be_bytes_mod_order(&outcome.total_spent));
        inputs.push(Fr::from_be_bytes_mod_order(&outcome.total_spent_salt));
        let Some(hash) = hasher.hash(&inputs).ok() else { return None; };

        let mut inputs: vec::Vec<Fr> = vec::Vec::<Fr>::new();
        inputs.push(Fr::from_be_bytes_mod_order(&outcome.new_results_commitment));
        inputs.push(Fr::from_be_bytes_mod_order(&hash.into_bigint().to_bytes_be()));
        let Some(hash) = hasher.hash(&inputs).ok() else { return None; };

        if hash.into_bigint().to_bytes_be() != self.state.commitment.tally.1 { return None; }

        Some(outcome_index)
    }

    fn prepare_public_inputs(
        self,
        coordinator: Coordinator,
        new_commitment: HashBytes
    ) -> Option<(VerifyKey, vec::Vec<Fr>, Commitment)>
    {
        let verify_key: VerifyKey;
        let mut inputs: vec::Vec<Fr> = vec::Vec::<Fr>::new();

        let message_batch_size: u32 = self.state.interactions.arity.pow(self.config.process_subtree_depth.into()).into();
        let mut current_batch_index = self.state.interactions.count;
        if current_batch_index > 0
        {
            let r = self.state.interactions.count % message_batch_size;
            if r == 0 { current_batch_index -= message_batch_size; }
            else { current_batch_index -= r; }
        }
        let mut proof_index = self.state.commitment.process.0;
        let index_offset = proof_index * message_batch_size;

        // Return inputs for message processing circuit
        if index_offset <= current_batch_index
        {
            verify_key = coordinator.verify_key.process;

            let Some(mut hasher) = Poseidon::<Fr>::new_circom(2).ok() else { return None; };
            let coord_pub_key = coordinator.public_key.clone();
            let coord_pub_key_fr: vec::Vec<Fr> = vec::Vec::from([ coord_pub_key.x, coord_pub_key.y ])
                .iter()
                .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
                .collect();
            let Some(coord_pub_key_hash) = hasher.hash(&coord_pub_key_fr).ok() else { return None; };
            let Some(root_bytes) = self.state.interactions.root else { return None; };

            current_batch_index -= index_offset;
            let mut end_batch_index = current_batch_index + message_batch_size;
            if end_batch_index > self.state.interactions.count { end_batch_index = self.state.interactions.count; }
            
            inputs.push(Fr::from(self.state.registrations.count + 1));
            inputs.push(Fr::from(self.get_voting_period_end()));
            inputs.push(Fr::from_be_bytes_mod_order(&root_bytes));
            inputs.push(Fr::from(self.state.registrations.depth));
            inputs.push(Fr::from(end_batch_index));
            inputs.push(Fr::from(current_batch_index));
            inputs.push(coord_pub_key_hash);
            inputs.push(Fr::from_be_bytes_mod_order(&self.state.commitment.process.1));
            inputs.push(Fr::from_be_bytes_mod_order(&new_commitment));

            let mut commitment = self.state.commitment.clone();
            commitment.process = (proof_index + 1, new_commitment);
    
            Some((verify_key, inputs, commitment))
        }

        // Return inputs for tally circuit
        else
        {
            proof_index = self.state.commitment.tally.0;
            verify_key = coordinator.verify_key.tally;

            let batch_size: u32 = self.state.registrations.arity.pow(self.config.tally_subtree_depth.into()).into();
            let current_batch_index = proof_index * batch_size;
            if current_batch_index >= self.state.registrations.count + 1 { return None; }

            inputs.push(Fr::from_be_bytes_mod_order(&self.state.commitment.process.1));
            inputs.push(Fr::from_be_bytes_mod_order(&self.state.commitment.tally.1));
            inputs.push(Fr::from_be_bytes_mod_order(&new_commitment));
            inputs.push(Fr::from(current_batch_index));
            inputs.push(Fr::from(self.state.registrations.count + 1));

            let mut commitment = self.state.commitment.clone();
            commitment.tally = (proof_index + 1, new_commitment);

            Some((verify_key, inputs, commitment))
        }
    }

    fn register_participant(
        mut self, 
        public_key: PublicKey,
        timestamp: u64
    ) -> Result<(u32, Self), MerkleTreeError>
    {
        let Some(mut hasher) = Poseidon::<Fr>::new_circom(4).ok() else { Err(MerkleTreeError::HashFailed)? };

        let mut inputs: vec::Vec<Fr> = vec::Vec::from([ public_key.x, public_key.y ])
            .iter()
            .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
            .collect();
        inputs.push(Fr::from(1));
        inputs.push(Fr::from(timestamp));

        let Some(result) = hasher.hash(&inputs).ok() else { Err(MerkleTreeError::HashFailed)? };
        let bytes = result.into_bigint().to_bytes_be();
        let mut leaf = [0u8; 32];
        leaf[..bytes.len()].copy_from_slice(&bytes);

        self.state.registrations = self.state.registrations.insert(leaf)?;

        Ok((self.state.registrations.count, self))
    }

    fn consume_interaction(
        mut self, 
        public_key: PublicKey,
        data: PollInteractionData
    ) -> Result<(u32, Self), MerkleTreeError>
    {
        let Some(mut hash4) = Poseidon::<Fr>::new_circom(4).ok() else { Err(MerkleTreeError::HashFailed)? };
        let Some(mut hash5) = Poseidon::<Fr>::new_circom(5).ok() else { Err(MerkleTreeError::HashFailed)? };

        let left_inputs: vec::Vec<Fr> = vec::Vec::from([ data[0], data[1], data[2], data[3], data[4] ])
            .iter()
            .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
            .collect();

        let right_inputs: vec::Vec<Fr> = vec::Vec::from([ data[5], data[6], data[7], data[8], data[9] ])
            .iter()
            .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
            .collect();

        let Some(left) = hash5.hash(&left_inputs).ok() else { Err(MerkleTreeError::HashFailed)? };
        let Some(right) = hash5.hash(&right_inputs).ok() else { Err(MerkleTreeError::HashFailed)? };

        let left_bytes = left.into_bigint().to_bytes_be();
        let right_bytes = right.into_bigint().to_bytes_be();

        let inputs: vec::Vec<Fr> = vec::Vec::from([
            left_bytes,
            right_bytes,
            vec::Vec::from(public_key.x),
            vec::Vec::from(public_key.y)
        ])
            .iter()
            .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
            .collect();

        let Some(result) = hash4.hash(&inputs).ok() else { Err(MerkleTreeError::HashFailed)? };

        let bytes = result.into_bigint().to_bytes_be();
        let mut leaf = [0u8; 32];
        leaf[..bytes.len()].copy_from_slice(&bytes);

        self.state.interactions = self.state.interactions.insert(leaf)?;

        Ok((self.state.interactions.count, self))
    }

    fn merge_registrations(
        mut self
    ) -> Result<Self, MerkleTreeError>
    {
        self.state.registrations = self.state.registrations.merge(false)?;

        let Some(root) = self.state.registrations.root else { Err(MerkleTreeError::MergeFailed)? };
        let Some(mut hasher) = Poseidon::<Fr>::new_circom(3).ok() else { Err(MerkleTreeError::HashFailed)? };

        let inputs: vec::Vec<Fr> = vec::Vec::from([ root, EMPTY_BALLOT_ROOTS[1], [0u8;32] ])
            .iter()
            .map(|bytes| Fr::from_be_bytes_mod_order(bytes))
            .collect();

        let Some(result) = hasher.hash(&inputs).ok() else { Err(MerkleTreeError::HashFailed)? };
        let bytes = result.into_bigint().to_bytes_be();
        let mut commitment = [0u8; 32];
        commitment[..bytes.len()].copy_from_slice(&bytes);

        self.state.commitment.process = (0, commitment);

        Ok(self)
    }

    fn merge_interactions(
        mut self
    ) -> Result<Self, MerkleTreeError>
    {
        self.state.interactions = self.state.interactions.merge(true)?;

        let process_batch_size: u32 = self.state.interactions.arity.pow(self.config.process_subtree_depth.into()).into();
        let process_extra_batch = if (self.state.interactions.count % process_batch_size) > 0 { 1 } else { 0 };
        self.state.commitment.expected_process = (self.state.interactions.count / process_batch_size) + process_extra_batch;

        let tally_batch_size: u32 = self.state.registrations.arity.pow(self.config.tally_subtree_depth.into()).into();
        self.state.commitment.expected_tally = 1 + (self.state.registrations.count / tally_batch_size);

        Ok(self)
    }

    fn registration_limit_reached(&self) -> bool
    {
        self.state.registrations.count >= self.config.max_registrations - 1
    }

    fn interaction_limit_reached(&self) -> bool
    {
        self.state.interactions.count >= self.config.max_interactions
    }

    /// Returns true iff poll is not None and `now` preceeds the end time of the poll.
    fn is_voting_period(&self) -> bool
    {
        let now = <frame_system::Pallet<T>>::block_number().saturated_into::<u64>();
        let voting_period_start = self.created_at + self.config.signup_period;
        let voting_period_end = voting_period_start + self.config.voting_period;
        now >= voting_period_start && now < voting_period_end
    }

    /// Returns true iff poll is currently within the registration period.
	fn is_registration_period(&self) -> bool
	{
		let now = <frame_system::Pallet<T>>::block_number().saturated_into::<u64>();
		now >= self.created_at && now < self.created_at + self.config.signup_period
	}

    fn get_voting_period_end(&self) -> BlockNumber
    {
        self.created_at + self.config.signup_period + self.config.voting_period
    }

    /// Returns true iff poll has ended.
    fn is_over(&self) -> bool
    {
		let now = <frame_system::Pallet<T>>::block_number().saturated_into::<u64>();
		now > self.get_voting_period_end()
    }

    /// Returns true iff poll outcome has been committed to state, or the poll is dead.
    fn is_fulfilled(&self) -> bool
    {
        self.state.outcome.is_some() || self.is_nullified()
    }

    fn is_merged(&self) -> bool
    {
        self.state.registrations.root.is_some() && self.state.interactions.root.is_some()
    }

    fn is_proven(&self) -> bool
    {
        (self.state.commitment.process.0 == self.state.commitment.expected_process) && 
            (self.state.commitment.tally.0 == self.state.commitment.expected_tally)
    }

    fn is_nullified(&self) -> bool
    {
        self.state.tombstone
    }

    fn nullify(mut self) -> Self
    {
        self.state.tombstone = true;
        self
    }
}

fn compute_merkle_root_from_path(
    depth: u8,
    index: u32,
    leaf: HashBytes,
    path: vec::Vec<vec::Vec<HashBytes>>
) -> Option<HashBytes>
{
    const VOTE_TREE_ARITY: u32 = 5;
    let Some(mut hasher) = Poseidon::<Fr>::new_circom(VOTE_TREE_ARITY as usize).ok() else { return None; };

    let mut idx = index;
    let mut position = idx % (VOTE_TREE_ARITY);
    let mut level = [[0u8; 32]; VOTE_TREE_ARITY as usize];
    let mut current = leaf;

    for i in 0..depth
    {
        for j in 0..VOTE_TREE_ARITY
        {
            if j == position { level[j as usize] = current; }
            else
            {
                let k = if j > position { j - 1 } else { j };
                level[j as usize] = path[i as usize][k as usize];
            }
        }

        let mut inputs: vec::Vec<Fr> = vec::Vec::new();
        for l in 0..VOTE_TREE_ARITY { inputs.push(Fr::from_be_bytes_mod_order(&level[l as usize])); }
        let Some(result) = hasher.hash(&inputs).ok() else { return None; };
        let bytes = result.into_bigint().to_bytes_be();
        let mut leaf = [0u8; 32];
        leaf[..bytes.len()].copy_from_slice(&bytes);

        idx /= VOTE_TREE_ARITY;
        position = idx % VOTE_TREE_ARITY;
        current = leaf;
    }

    Some(current)
}
