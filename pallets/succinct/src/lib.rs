#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{pallet_prelude::*, parameter_types, Deserialize, Serialize};
use sp_core::{H256, U256};

pub use pallet::*;

use crate::verifier::Verifier;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
// mod verify;
mod state;
pub(crate) mod verifier;
mod weights;

type VerificationKeyDef<T> = BoundedVec<u8, <T as Config>::MaxVerificationKeyLength>;

// TODO remove unused and define correct values
parameter_types! {
	pub const MinSyncCommitteeParticipants: u16=10;
	pub const SyncCommitteeSize: u32=512;
	pub const FinalizedRootIndex: u32=105;
	pub const NextSyncCommitteeIndex: u32= 55;
	pub const ExecutionStateRootIndex: u32= 402;
	pub const MaxPublicInputsLength: u32 = 9;
	pub const MaxVerificationKeyLength: u32 = 4143;
	pub const MaxProofLength: u32 = 1133;
	pub const StepFunctionId: H256 = H256([0u8; 32]);
	pub const RotateFunctionId: H256 = H256([0u8; 32]);
}

#[frame_support::pallet]
pub mod pallet {
	use ark_std::string::String;
	use ark_std::string::ToString;
	use ark_std::{vec, vec::Vec};
	use ethabi::{Bytes, ParamType, Token, Uint};
	use frame_support::dispatch::{GetDispatchInfo, UnfilteredDispatchable};
	use frame_support::traits::{Hash, UnixTime};
	use frame_support::{pallet_prelude::ValueQuery, DefaultNoBound};
	use sp_core::H256;
	use sp_io::hashing::sha2_256;

	use frame_system::pallet_prelude::*;
	pub use weights::WeightInfo;

	use crate::state::{parse_step_output, LightClientStep, State, VerifiedCallStore};
	use crate::verifier::zk_light_client_rotate;
	use crate::verifier::zk_light_client_step;

	use super::*;

	#[pallet::error]
	pub enum Error<T> {
		UpdaterMisMatch,
		VerificationError,
		CannotUpdateStateStorage,
		UpdateSlotIsFarInTheFuture,
		UpdateSlotLessThanCurrentHead,
		NotEnoughParticipants,
		SyncCommitteeNotInitialized,
		NotEnoughSyncCommitteeParticipants,
		// verification
		TooLongVerificationKey,
		ProofIsEmpty,
		VerificationKeyIsNotSet,
		MalformedVerificationKey,
		NotSupportedCurve,
		NotSupportedProtocol,
		ProofCreationError,
		InvalidRotateProof,
		InvalidStepProof,

		//
		StepVerificationError,
		HeaderRootNotSet,
		VerificationFailed,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub (super) fn deposit_event)]
	pub enum Event<T: Config> {
		// emit event once the head is updated
		HeadUpdate {
			slot: u64,
			finalization_root: H256,
		},
		// emit event once the sync committee updates
		SyncCommitteeUpdate {
			period: u64,
			root: U256,
		},
		// emit event when verification setup is completed
		VerificationSetupCompleted,
		// emit event if verification is success
		VerificationSuccess {
			who: H256,
			attested_slot: u64,
			finalized_slot: u64,
		},
		// emit when new updater is set
		NewUpdater {
			old: H256,
			new: H256,
		},
	}

	// Storage definitions

	//TODO step and rotate verification keys can be stored as constants and not in the storage which can simplify implementation.
	#[pallet::storage]
	pub type StepVerificationKeyStorage<T: Config> =
		StorageValue<_, VerificationKeyDef<T>, ValueQuery>;

	#[pallet::storage]
	pub type RotateVerificationKeyStorage<T: Config> =
		StorageValue<_, VerificationKeyDef<T>, ValueQuery>;

	// Storage for a general state.
	#[pallet::storage]
	pub type StateStorage<T: Config> = StorageValue<_, State, ValueQuery>;

	// Maps from a slot to a block header root.
	#[pallet::storage]
	#[pallet::getter(fn get_header)]
	pub type Headers<T> = StorageMap<_, Identity, u64, H256, ValueQuery>;

	// Maps slot to the timestamp of when the headers mapping was updated with slot as a key
	#[pallet::storage]
	#[pallet::getter(fn get_timestamp)]
	pub type Timestamps<T> = StorageMap<_, Identity, u64, u64, ValueQuery>;

	// Maps from a slot to the current finalized ethereum execution state root.
	#[pallet::storage]
	#[pallet::getter(fn get_state_root)]
	pub type ExecutionStateRoots<T> = StorageMap<_, Identity, u64, H256, ValueQuery>;

	// Maps from a period to the poseidon commitment for the sync committee.
	#[pallet::storage]
	#[pallet::getter(fn get_poseidon)]
	pub type SyncCommitteePoseidons<T> = StorageMap<_, Identity, u64, U256, ValueQuery>;

	#[pallet::storage]
	pub type VerifiedCall<T> = StorageValue<_, VerifiedCallStore, ValueQuery>;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
		type TimeProvider: UnixTime;
		#[pallet::constant]
		type MaxPublicInputsLength: Get<u32>;
		// 9
		#[pallet::constant]
		type MaxProofLength: Get<u32>;
		// 1133
		#[pallet::constant]
		type MaxVerificationKeyLength: Get<u32>;
		// 4143
		#[pallet::constant]
		type MinSyncCommitteeParticipants: Get<u32>;
		#[pallet::constant]
		type SyncCommitteeSize: Get<u32>;
		#[pallet::constant]
		type FinalizedRootIndex: Get<u32>;
		#[pallet::constant]
		type NextSyncCommitteeIndex: Get<u32>;
		#[pallet::constant]
		type ExecutionStateRootIndex: Get<u32>;

		#[pallet::constant]
		type StepFunctionId: Get<H256>;

		#[pallet::constant]
		type RotateFunctionId: Get<H256>;

		type RuntimeCall: Parameter
			+ UnfilteredDispatchable<RuntimeOrigin = Self::RuntimeOrigin>
			+ GetDispatchInfo;

		type WeightInfo: WeightInfo;
	}

	//  pallet initialization data
	// TODO check if genesis is a good place for this
	#[pallet::genesis_config]
	#[derive(DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		pub updater: Hash,
		pub genesis_validators_root: Hash,
		pub genesis_time: u64,
		pub seconds_per_slot: u64,
		pub slots_per_period: u64,
		pub source_chain_id: u32,
		pub finality_threshold: u16,
		pub consistent: bool,
		pub head: u64,
		pub _phantom: PhantomData<T>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		// TODO init state
		fn build(&self) {
			// TODO time cannot be called at Genesis
			// T::TimeProvider::now().as_secs()
			// Preconfigure init data
			<StateStorage<T>>::put(State {
				updater: self.updater,
				genesis_validators_root: H256::zero(),
				genesis_time: 1696440023,
				seconds_per_slot: 12000,
				slots_per_period: 8192,
				source_chain_id: 1,
				finality_threshold: 290,
				head: 0,
				consistent: true,
			});

			let s = U256::from_dec_str(
				"7032059424740925146199071046477651269705772793323287102921912953216115444414",
			)
			.unwrap();
			<SyncCommitteePoseidons<T>>::insert(0u64, s);
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		[u8; 32]: From<T::AccountId>,
	{
		/// Sets the sync committee for the next sync committee period.
		/// A commitment to the the next sync committee is signed by the current sync committee.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::rotate())]
		pub fn rotate(origin: OriginFor<T>, update: state::LightClientRotate) -> DispatchResult {
			let sender: [u8; 32] = ensure_signed(origin)?.into();
			let state = StateStorage::<T>::get();
			ensure!(H256(sender) == state.updater, Error::<T>::UpdaterMisMatch);

			let step = &update.step;

			let finalized = process_step::<T>(state, step)?;
			let current_period = step.finalized_slot / state.slots_per_period;
			let next_period = current_period + 1;

			let verifier = get_rotate_verifier::<T>()?;

			// proof verification
			let success = zk_light_client_rotate(&update, verifier)
				.map_err(|_| Error::<T>::VerificationError)?;

			ensure!(success, Error::<T>::InvalidRotateProof);

			Self::deposit_event(Event::VerificationSuccess {
				who: sender.into(),
				attested_slot: step.attested_slot,
				finalized_slot: step.finalized_slot,
			});
			if finalized {
				let is_set =
					set_sync_committee_poseidon::<T>(next_period, update.sync_committee_poseidon)?;
				if is_set {
					Self::deposit_event(Event::SyncCommitteeUpdate {
						period: next_period,
						root: update.sync_committee_poseidon,
					});
				}
			}

			Ok(())
		}

		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn step_refactor(origin: OriginFor<T>, attested_slot: u64) -> DispatchResult {
			let sender: [u8; 32] = ensure_signed(origin)?.into();
			let state = StateStorage::<T>::get();
			// ensure sender is preconfigured
			ensure!(H256(sender) == state.updater, Error::<T>::UpdaterMisMatch);

			let current_period = attested_slot / state.slots_per_period;
			let sc_poseidon = SyncCommitteePoseidons::<T>::get(current_period);

			let input = ethabi::encode(&[
				Token::Uint(sc_poseidon),
				Token::Uint(Uint::from(attested_slot)),
			]);
			// let result = verified_call::<T>(attested_slot, StepFunctionId::get(), input)?;
			//
			// let finalized_header_root = result.finalized_header_root;
			// let execution_state_root = result.execution_state_root;
			// let finalized_slot = result.finalized_slot;
			// let participation = result.participation;
			//
			// ensure!(participation >= state.finality_threshold, Error::<T>::NotEnoughParticipants);
			//
			// let updated = set_slot_roots::<T>(
			//     finalized_slot,
			//     finalized_header_root,
			//     execution_state_root,
			// )?;

			// TODO true false or error?
			Ok(())
		}

		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn fulfill_call(
			origin: OriginFor<T>,
			function_id: H256,
			input: Vec<u8>,
			output: Vec<u8>,
			proof: Vec<u8>,
			//                         callback contract
			//                         callback data
		) -> DispatchResult {
			let sender: [u8; 32] = ensure_signed(origin)?.into();
			let state = StateStorage::<T>::get();
			// ensure sender is preconfigured
			ensure!(H256(sender) == state.updater, Error::<T>::UpdaterMisMatch);
			let input_hash = H256(sha2_256(input.as_slice()));
			let output_hash = H256(sha2_256(output.as_slice()));

			let verifier = get_verifier::<T>(function_id)?;

			let success = verifier
				.verify_proof_refactor(input_hash, output_hash, proof)
				.map_err(|_| Error::<T>::VerificationError)?;

			ensure!(success, Error::<T>::VerificationFailed);

			let v = VerifiedCallStore {
				verified_function_id: function_id,
				verified_input_hash: input_hash,
				verified_output: parse_step_output(output),
			};

			// VerifiedCall::<T>::set(v);

			Ok(())
		}

		// #[pallet::call_index(7)]
		// #[pallet::weight(T::WeightInfo::rotate())]
		// pub fn rotate_refactor(origin: OriginFor<T>, finalized_slot: u64) -> DispatchResult {
		//     let sender: [u8; 32] = ensure_signed(origin)?.into();
		//     let state = StateStorage::<T>::get();
		//     ensure!(H256(sender) == state.updater, Error::<T>::UpdaterMisMatch);
		//
		//     let finalized_header_root = Headers::<T>::get(finalized_slot);
		//     ensure!(finalized_header_root != H256::zero(), Error::<T>::HeaderRootNotSet);
		//
		//     let input = ethabi::encode(&[Token::FixedBytes(finalized_header_root.0.to_vec())]);
		//     let result: VerifiedOutput = verified_call::<T>(finalized_slot, RotateFunctionId::get(), input)?;
		//
		//     let sync_committee_poseidon = result.sync_committee_poseidon;
		//
		//     let current_period = finalized_slot / state.slots_per_period;
		//     let next_period = current_period + 1;
		//
		//     let is_set =
		//         set_sync_committee_poseidon::<T>(next_period, sync_committee_poseidon)?;
		//     if is_set {
		//         Self::deposit_event(Event::SyncCommitteeUpdate {
		//             period: next_period,
		//             root: update.sync_committee_poseidon,
		//         });
		//     }
		//
		//     //
		//     // let step = &update.step;
		//     //
		//     // let finalized = process_step::<T>(state, step)?;
		//     // let current_period = step.finalized_slot / state.slots_per_period;
		//     // let next_period = current_period + 1;
		//     //
		//     // let verifier = get_rotate_verifier::<T>()?;
		//     //
		//     // // proof verification
		//     // let success = zk_light_client_rotate(&update, verifier)
		//     //     .map_err(|_| Error::<T>::VerificationError)?;
		//     //
		//     // ensure!(success, Error::<T>::InvalidRotateProof);
		//
		//     // Self::deposit_event(Event::VerificationSuccess {
		//     //     who: sender.into(),
		//     //     attested_slot: step.attested_slot,
		//     //     finalized_slot: step.finalized_slot,
		//     // });
		//     // if finalized {
		//     //     let is_set =
		//     //         set_sync_committee_poseidon::<T>(next_period, update.sync_committee_poseidon)?;
		//     //     if is_set {
		//     //         Self::deposit_event(Event::SyncCommitteeUpdate {
		//     //             period: next_period,
		//     //             root: update.sync_committee_poseidon,
		//     //         });
		//     //     }
		//     // }
		//
		//     Ok(())
		// }

		/// Updates the head of the light client to the provided slot.
		/// The conditions for updating the head of the light client involve checking:
		///      1) Enough signatures from the current sync committee for n=512
		///      2) A valid finality proof
		///      3) A valid execution state root proof
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn step(origin: OriginFor<T>, update: LightClientStep) -> DispatchResult {
			let sender: [u8; 32] = ensure_signed(origin)?.into();
			let state = StateStorage::<T>::get();
			// ensure sender is preconfigured
			ensure!(H256(sender) == state.updater, Error::<T>::UpdaterMisMatch);

			let finalized = process_step::<T>(state, &update)?;

			let block_time: u64 = T::TimeProvider::now().as_secs();
			let current_slot = (block_time - state.genesis_time) / state.seconds_per_slot;

			ensure!(
				current_slot >= update.attested_slot,
				Error::<T>::UpdateSlotIsFarInTheFuture
			);

			ensure!(
				update.finalized_slot >= state.head,
				Error::<T>::UpdateSlotLessThanCurrentHead
			);

			ensure!(finalized, Error::<T>::NotEnoughParticipants);

			let updated = set_slot_roots::<T>(
				update.finalized_slot,
				update.finalized_header_root,
				update.execution_state_root,
			)?;
			if updated {
				Self::deposit_event(Event::HeadUpdate {
					slot: update.finalized_slot,
					finalization_root: update.finalized_header_root,
				});
			}

			Ok(())
		}

		/// Sets updater that can call step and rotate functions
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn set_updater(origin: OriginFor<T>, updater: H256) -> DispatchResult {
			ensure_root(origin)?;
			let old = StateStorage::<T>::get();
			StateStorage::<T>::try_mutate(|cfg| -> Result<(), DispatchError> {
				cfg.updater = updater;
				Ok(())
			})?;

			Self::deposit_event(Event::<T>::NewUpdater {
				old: old.updater,
				new: updater,
			});
			Ok(())
		}

		/// Sets verification public inputs for step function.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn setup_step_verification(
			origin: OriginFor<T>,
			verification: String,
		) -> DispatchResult {
			ensure_root(origin)?;
			// try from json to Verifier struct
			Verifier::from_json_u8_slice(verification.as_bytes())
				.map_err(|_| Error::<T>::MalformedVerificationKey)?;
			// store verification to storage
			store_step_verification_key::<T>(verification.as_bytes().to_vec())?;

			Self::deposit_event(Event::<T>::VerificationSetupCompleted);
			Ok(())
		}

		/// Sets verification public inputs for rotate function.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::step())]
		pub fn setup_rotate_verification(
			origin: OriginFor<T>,
			verification: String,
		) -> DispatchResult {
			ensure_root(origin)?;
			// try from json to Verifier struct
			Verifier::from_json_u8_slice(verification.as_bytes())
				.map_err(|_| Error::<T>::MalformedVerificationKey)?;
			// store verification to storage
			store_rotate_verification_key::<T>(verification.as_bytes().to_vec())?;

			Self::deposit_event(Event::<T>::VerificationSetupCompleted);
			Ok(())
		}
	}

	fn set_slot_roots<T: Config>(
		slot: u64,
		finalized_header_root: H256,
		execution_state_root: H256,
	) -> Result<bool, DispatchError> {
		let header = Headers::<T>::get(slot);

		if header != H256::zero() && header != finalized_header_root {
			StateStorage::<T>::try_mutate(|m| -> Result<(), DispatchError> {
				m.consistent = false;
				Ok(())
			})
			.map_err(|_| Error::<T>::CannotUpdateStateStorage)?;
			return Ok(false);
		}
		let state_root = ExecutionStateRoots::<T>::get(slot);

		if state_root != H256::zero() && state_root != execution_state_root {
			StateStorage::<T>::try_mutate(|m| -> Result<(), DispatchError> {
				m.consistent = false;
				Ok(())
			})
			.map_err(|_| Error::<T>::CannotUpdateStateStorage)?;
			return Ok(false);
		}

		StateStorage::<T>::try_mutate(|m| -> Result<(), DispatchError> {
			m.head = slot;
			Ok(())
		})
		.map_err(|_| Error::<T>::CannotUpdateStateStorage)?;

		Headers::<T>::insert(slot, finalized_header_root);

		// TODO can this time be used as block time?
		Timestamps::<T>::insert(slot, T::TimeProvider::now().as_secs());

		Ok(true)
	}

	fn set_sync_committee_poseidon<T: Config>(
		period: u64,
		poseidon: U256,
	) -> Result<bool, DispatchError> {
		let sync_committee_poseidons = SyncCommitteePoseidons::<T>::get(period);

		if poseidon != U256::zero() && sync_committee_poseidons != poseidon {
			StateStorage::<T>::try_mutate(|m| -> Result<(), DispatchError> {
				m.consistent = false;
				Ok(())
			})
			.map_err(|_| Error::<T>::CannotUpdateStateStorage)?;
			return Ok(false);
		}
		SyncCommitteePoseidons::<T>::set(period, poseidon);

		Ok(true)
	}

	fn process_step<T: Config>(
		state: State,
		update: &LightClientStep,
	) -> Result<bool, DispatchError> {
		let current_period = update.finalized_slot / state.slots_per_period; //get_sync_committee_period(state, update.attested_slot);
		let sc_poseidon = SyncCommitteePoseidons::<T>::get(current_period);

		ensure!(
			sc_poseidon != U256::zero(),
			Error::<T>::SyncCommitteeNotInitialized
		);
		ensure!(
			update.participation >= MinSyncCommitteeParticipants::get(),
			Error::<T>::NotEnoughSyncCommitteeParticipants
		);

		let verifier = get_step_verifier::<T>()?;

		let success = zk_light_client_step(&update, sc_poseidon, verifier)
			.map_err(|_| Error::<T>::VerificationError)?;

		ensure!(success, Error::<T>::InvalidStepProof);
		Ok(update.participation > state.finality_threshold)
	}

	fn get_verifier<T: Config>(function_id: H256) -> Result<Verifier, Error<T>> {
		return if function_id == StepFunctionId::get() {
			get_step_verifier()
		} else {
			get_rotate_verifier()
		};
	}

	fn get_step_verifier<T: Config>() -> Result<Verifier, Error<T>> {
		let vk = StepVerificationKeyStorage::<T>::get();
		ensure!(!vk.is_empty(), Error::<T>::VerificationKeyIsNotSet);
		let deserialized_vk = Verifier::from_json_u8_slice(vk.as_slice())
			.map_err(|_| Error::<T>::MalformedVerificationKey)?;
		Ok(deserialized_vk)
	}

	fn get_rotate_verifier<T: Config>() -> Result<Verifier, Error<T>> {
		let vk = RotateVerificationKeyStorage::<T>::get();
		ensure!(!vk.is_empty(), Error::<T>::VerificationKeyIsNotSet);
		let deserialized_vk = Verifier::from_json_u8_slice(vk.as_slice())
			.map_err(|_| Error::<T>::MalformedVerificationKey)?;
		Ok(deserialized_vk)
	}

	fn store_step_verification_key<T: Config>(vec_vk: Vec<u8>) -> Result<Verifier, Error<T>> {
		let vk: VerificationKeyDef<T> = vec_vk
			.try_into()
			.map_err(|_| Error::<T>::TooLongVerificationKey)?;
		let deserialized_vk = Verifier::from_json_u8_slice(vk.as_slice())
			.map_err(|_| Error::<T>::MalformedVerificationKey)?;
		ensure!(
			deserialized_vk.vk_json.curve == "bn128".to_string(),
			Error::<T>::NotSupportedCurve
		);
		ensure!(
			deserialized_vk.vk_json.protocol == "groth16".to_string(),
			Error::<T>::NotSupportedProtocol
		);

		StepVerificationKeyStorage::<T>::put(vk);
		Ok(deserialized_vk)
	}

	fn store_rotate_verification_key<T: Config>(vec_vk: Vec<u8>) -> Result<Verifier, Error<T>> {
		let vk: VerificationKeyDef<T> = vec_vk
			.try_into()
			.map_err(|_| Error::<T>::TooLongVerificationKey)?;
		let deserialized_vk = Verifier::from_json_u8_slice(vk.as_slice())
			.map_err(|_| Error::<T>::MalformedVerificationKey)?;
		ensure!(
			deserialized_vk.vk_json.curve == "bn128".to_string(),
			Error::<T>::NotSupportedCurve
		);
		ensure!(
			deserialized_vk.vk_json.protocol == "groth16".to_string(),
			Error::<T>::NotSupportedProtocol
		);

		RotateVerificationKeyStorage::<T>::put(vk);
		Ok(deserialized_vk)
	}

	// fn verified_call<T: Config>(attested_slot: u64, function_id: H256, input: Bytes) -> Result<dyn VerifiedOutput, DispatchError> {
	//     let input_hash = sha2_256(input.as_slice());
	//     let verified_call = VerifiedCall::<T>::get();
	//     if verified_call.verified_function_id == function_id && verified_call.verified_input_hash == H256(input_hash) {
	//         let trait_object: &dyn VerifiedOutput = &verified_call.verified_output;
	//         Ok(trait_object)
	//     } else {
	//         return Err(Error::<T>::StepVerificationError.into());
	//     }
	// }
}