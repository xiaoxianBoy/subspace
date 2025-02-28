// Copyright (C) 2021 Subspace Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Pallet Domains

#![cfg_attr(not(feature = "std"), no_std)]
#![feature(array_windows, associated_type_bounds, let_chains, variant_count)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(test)]
mod tests;

pub mod block_tree;
mod bundle_storage_fund;
pub mod domain_registry;
pub mod runtime_registry;
mod staking;
mod staking_epoch;
pub mod weights;

extern crate alloc;

use crate::block_tree::verify_execution_receipt;
use crate::bundle_storage_fund::storage_fund_account;
use crate::domain_registry::Error as DomainRegistryError;
use crate::staking::OperatorStatus;
use crate::staking_epoch::EpochTransitionResult;
use crate::weights::WeightInfo;
#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use codec::{Decode, Encode};
use frame_support::ensure;
use frame_support::pallet_prelude::StorageVersion;
use frame_support::traits::fungible::{Inspect, InspectHold};
use frame_support::traits::tokens::{Fortitude, Preservation};
use frame_support::traits::{Get, Randomness as RandomnessT};
use frame_support::weights::Weight;
use frame_system::offchain::SubmitTransaction;
use frame_system::pallet_prelude::*;
pub use pallet::*;
use scale_info::TypeInfo;
use sp_consensus_subspace::consensus::is_proof_of_time_valid;
use sp_consensus_subspace::WrappedPotOutput;
use sp_core::H256;
use sp_domains::bundle_producer_election::BundleProducerElectionParams;
use sp_domains::{
    DomainBlockLimit, DomainBundleLimit, DomainId, DomainInstanceData, ExecutionReceipt,
    OpaqueBundle, OperatorId, OperatorPublicKey, RuntimeId,
    DOMAIN_EXTRINSICS_SHUFFLING_SEED_SUBJECT, EMPTY_EXTRINSIC_ROOT,
};
use sp_domains_fraud_proof::fraud_proof::{
    FraudProof, InvalidBlockFeesProof, InvalidDomainBlockHashProof,
};
use sp_domains_fraud_proof::verification::{
    verify_bundle_equivocation_fraud_proof, verify_invalid_block_fees_fraud_proof,
    verify_invalid_bundles_fraud_proof, verify_invalid_domain_block_hash_fraud_proof,
    verify_invalid_domain_extrinsics_root_fraud_proof, verify_invalid_state_transition_fraud_proof,
    verify_invalid_transfers_fraud_proof, verify_valid_bundle_fraud_proof,
};
use sp_runtime::traits::{BlockNumberProvider, CheckedSub, Hash, Header, One, Zero};
use sp_runtime::transaction_validity::TransactionPriority;
use sp_runtime::{RuntimeAppPublic, SaturatedConversion, Saturating};
pub use staking::OperatorConfig;
use subspace_core_primitives::{BlockHash, PotOutput, SlotNumber, U256};
use subspace_runtime_primitives::Balance;

pub(crate) type BalanceOf<T> =
    <<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;

pub(crate) type FungibleHoldId<T> =
    <<T as Config>::Currency as InspectHold<<T as frame_system::Config>::AccountId>>::Reason;

pub(crate) type NominatorId<T> = <T as frame_system::Config>::AccountId;

pub trait HoldIdentifier<T: Config> {
    fn staking_staked(operator_id: OperatorId) -> FungibleHoldId<T>;
    fn domain_instantiation_id(domain_id: DomainId) -> FungibleHoldId<T>;
    fn storage_fund_withdrawal(operator_id: OperatorId) -> FungibleHoldId<T>;
}

pub trait BlockSlot<T: frame_system::Config> {
    // Return the future slot of the given `block_number`
    fn future_slot(block_number: BlockNumberFor<T>) -> Option<sp_consensus_slots::Slot>;

    // Return the latest block number whose slot is less than the given `to_check` slot
    fn slot_produced_after(to_check: sp_consensus_slots::Slot) -> Option<BlockNumberFor<T>>;
}

pub type ExecutionReceiptOf<T> = ExecutionReceipt<
    BlockNumberFor<T>,
    <T as frame_system::Config>::Hash,
    DomainBlockNumberFor<T>,
    <T as Config>::DomainHash,
    BalanceOf<T>,
>;

pub type OpaqueBundleOf<T> = OpaqueBundle<
    BlockNumberFor<T>,
    <T as frame_system::Config>::Hash,
    <T as Config>::DomainHeader,
    BalanceOf<T>,
>;

/// Parameters used to verify proof of election.
#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub(crate) struct ElectionVerificationParams<Balance> {
    operators: BTreeMap<OperatorId, Balance>,
    total_domain_stake: Balance,
}

#[derive(Debug, Decode, Encode, TypeInfo, PartialEq, Eq, Clone)]
pub(crate) enum FraudProofTag {
    BadER(DomainId),
    BundleEquivocation(OperatorId),
}

pub type DomainBlockNumberFor<T> = <<T as Config>::DomainHeader as Header>::Number;
pub type DomainHashingFor<T> = <<T as Config>::DomainHeader as Header>::Hashing;
pub type ReceiptHashFor<T> = <<T as Config>::DomainHeader as Header>::Hash;

pub type BlockTreeNodeFor<T> = crate::block_tree::BlockTreeNode<
    BlockNumberFor<T>,
    <T as frame_system::Config>::Hash,
    DomainBlockNumberFor<T>,
    <T as Config>::DomainHash,
    BalanceOf<T>,
>;

/// The current storage version.
const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

/// The number of bundle of a particular domain to be included in the block is probabilistic
/// and based on the consensus chain slot probability and domain bundle slot probability, usually
/// the value is 6 on average, smaller/bigger value with less probability, we hypocritically use
/// 100 as the maximum number of bundle per block for benchmarking.
const MAX_BUNLDE_PER_BLOCK: u32 = 100;

#[frame_support::pallet]
mod pallet {
    #![allow(clippy::large_enum_variant)]

    use crate::block_tree::{
        execution_receipt_type, process_execution_receipt, Error as BlockTreeError, ReceiptType,
    };
    #[cfg(not(feature = "runtime-benchmarks"))]
    use crate::block_tree::{prune_receipt, AcceptedReceiptType};
    #[cfg(not(feature = "runtime-benchmarks"))]
    use crate::bundle_storage_fund::refund_storage_fee;
    use crate::bundle_storage_fund::{charge_bundle_storage_fee, Error as BundleStorageFundError};
    use crate::domain_registry::{
        do_instantiate_domain, do_update_domain_allow_list, DomainConfig, DomainObject,
        Error as DomainRegistryError,
    };
    use crate::runtime_registry::{
        do_register_runtime, do_schedule_runtime_upgrade, do_upgrade_runtimes,
        register_runtime_at_genesis, Error as RuntimeRegistryError, RuntimeObject,
        ScheduledRuntimeUpgrade,
    };
    #[cfg(not(feature = "runtime-benchmarks"))]
    use crate::staking::do_reward_operators;
    use crate::staking::{
        do_deregister_operator, do_nominate_operator, do_register_operator, do_slash_operators,
        do_unlock_funds, do_unlock_operator, do_withdraw_stake, Deposit, DomainEpoch,
        Error as StakingError, Operator, OperatorConfig, SharePrice, StakingSummary, Withdrawal,
    };
    use crate::staking_epoch::{do_finalize_domain_current_epoch, Error as StakingEpochError};
    use crate::weights::WeightInfo;
    #[cfg(not(feature = "runtime-benchmarks"))]
    use crate::DomainHashingFor;
    use crate::{
        BalanceOf, BlockSlot, BlockTreeNodeFor, DomainBlockNumberFor, ElectionVerificationParams,
        HoldIdentifier, NominatorId, OpaqueBundleOf, ReceiptHashFor, MAX_BUNLDE_PER_BLOCK,
        STORAGE_VERSION,
    };
    #[cfg(not(feature = "std"))]
    use alloc::string::String;
    #[cfg(not(feature = "std"))]
    use alloc::vec::Vec;
    use codec::FullCodec;
    use domain_runtime_primitives::EVMChainId;
    use frame_support::pallet_prelude::*;
    use frame_support::traits::fungible::{InspectHold, Mutate, MutateHold};
    use frame_support::traits::Randomness as RandomnessT;
    use frame_support::weights::Weight;
    use frame_support::{Identity, PalletError};
    use frame_system::pallet_prelude::*;
    use sp_consensus_slots::Slot;
    use sp_core::H256;
    use sp_domains::bundle_producer_election::ProofOfElectionError;
    use sp_domains::{
        BundleDigest, ConfirmedDomainBlock, DomainBundleSubmitted, DomainId,
        DomainsTransfersTracker, EpochIndex, GenesisDomain, OperatorAllowList, OperatorId,
        OperatorPublicKey, RuntimeId, RuntimeType,
    };
    use sp_domains_fraud_proof::fraud_proof::FraudProof;
    use sp_domains_fraud_proof::InvalidTransactionCode;
    use sp_runtime::traits::{
        AtLeast32BitUnsigned, BlockNumberProvider, CheckEqual, CheckedAdd, Header as HeaderT,
        MaybeDisplay, One, SimpleBitOps, Zero,
    };
    use sp_runtime::Saturating;
    use sp_std::boxed::Box;
    use sp_std::collections::btree_set::BTreeSet;
    use sp_std::fmt::Debug;
    use sp_std::vec;
    use subspace_core_primitives::U256;
    use subspace_runtime_primitives::StorageFee;

    #[pallet::config]
    pub trait Config: frame_system::Config<Hash: Into<H256>> {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

        // TODO: `DomainHash` can be derived from `DomainHeader`, it is still needed just for
        // converting `DomainHash` to/from `H256` without encode/decode, remove it once we found
        // other ways to do this.
        /// Domain block hash type.
        type DomainHash: Parameter
            + Member
            + MaybeSerializeDeserialize
            + Debug
            + MaybeDisplay
            + SimpleBitOps
            + Ord
            + Default
            + Copy
            + CheckEqual
            + sp_std::hash::Hash
            + AsRef<[u8]>
            + AsMut<[u8]>
            + MaxEncodedLen
            + Into<H256>
            + From<H256>;

        /// The domain header type.
        type DomainHeader: HeaderT<Hash = Self::DomainHash>;

        /// Same with `pallet_subspace::Config::ConfirmationDepthK`.
        #[pallet::constant]
        type ConfirmationDepthK: Get<BlockNumberFor<Self>>;

        /// Delay before a domain runtime is upgraded.
        #[pallet::constant]
        type DomainRuntimeUpgradeDelay: Get<BlockNumberFor<Self>>;

        /// Currency type used by the domains for staking and other currency related stuff.
        type Currency: Mutate<Self::AccountId>
            + InspectHold<Self::AccountId>
            + MutateHold<Self::AccountId>;

        /// Type representing the shares in the staking protocol.
        type Share: Parameter
            + Member
            + MaybeSerializeDeserialize
            + Debug
            + AtLeast32BitUnsigned
            + FullCodec
            + Copy
            + Default
            + TypeInfo
            + MaxEncodedLen
            + IsType<BalanceOf<Self>>;

        /// A variation of the Identifier used for holding the funds used for staking and domains.
        type HoldIdentifier: HoldIdentifier<Self>;

        /// The block tree pruning depth.
        #[pallet::constant]
        type BlockTreePruningDepth: Get<DomainBlockNumberFor<Self>>;

        /// Consensus chain slot probability.
        #[pallet::constant]
        type ConsensusSlotProbability: Get<(u64, u64)>;

        /// The maximum block size limit for all domain.
        #[pallet::constant]
        type MaxDomainBlockSize: Get<u32>;

        /// The maximum block weight limit for all domain.
        #[pallet::constant]
        type MaxDomainBlockWeight: Get<Weight>;

        /// The maximum bundle per block limit for all domain.
        #[pallet::constant]
        type MaxBundlesPerBlock: Get<u32>;

        /// The maximum domain name length limit for all domain.
        #[pallet::constant]
        type MaxDomainNameLength: Get<u32>;

        /// The amount of fund to be locked up for the domain instance creator.
        #[pallet::constant]
        type DomainInstantiationDeposit: Get<BalanceOf<Self>>;

        /// Weight information for extrinsics in this pallet.
        type WeightInfo: WeightInfo;

        /// Initial domain tx range value.
        #[pallet::constant]
        type InitialDomainTxRange: Get<u64>;

        /// Domain tx range is adjusted after every DomainTxRangeAdjustmentInterval blocks.
        #[pallet::constant]
        type DomainTxRangeAdjustmentInterval: Get<u64>;

        /// Minimum operator stake required to become operator of a domain.
        #[pallet::constant]
        type MinOperatorStake: Get<BalanceOf<Self>>;

        /// Minimum nominator stake required to nominate and operator.
        #[pallet::constant]
        type MinNominatorStake: Get<BalanceOf<Self>>;

        /// Minimum number of blocks after which any finalized withdrawals are released to nominators.
        #[pallet::constant]
        type StakeWithdrawalLockingPeriod: Get<DomainBlockNumberFor<Self>>;

        /// Domain epoch transition interval
        #[pallet::constant]
        type StakeEpochDuration: Get<DomainBlockNumberFor<Self>>;

        /// Treasury account.
        #[pallet::constant]
        type TreasuryAccount: Get<Self::AccountId>;

        /// The maximum number of pending staking operation that can perform upon epoch transition.
        #[pallet::constant]
        type MaxPendingStakingOperation: Get<u32>;

        /// The maximum number of nominators for given operator.
        #[pallet::constant]
        type MaxNominators: Get<u32>;

        /// Randomness source.
        type Randomness: RandomnessT<Self::Hash, BlockNumberFor<Self>>;

        /// The pallet-domains's pallet id.
        #[pallet::constant]
        type PalletId: Get<frame_support::PalletId>;

        /// Storage fee interface used to deal with bundle storage fee
        type StorageFee: StorageFee<BalanceOf<Self>>;

        /// The block slot
        type BlockSlot: BlockSlot<Self>;

        /// Transfers tracker.
        type DomainsTransfersTracker: DomainsTransfersTracker<BalanceOf<Self>>;

        /// Upper limit for total initial accounts domains
        type MaxInitialDomainAccounts: Get<u32>;

        /// Minimum balance for each initial domain account
        type MinInitialDomainAccountBalance: Get<BalanceOf<Self>>;

        /// How many block a bundle should still consider as valid after produced
        #[pallet::constant]
        type BundleLongevity: Get<u32>;

        /// Post hook to notify accepted domain bundles in previous block.
        type DomainBundleSubmitted: DomainBundleSubmitted;
    }

    #[pallet::pallet]
    #[pallet::without_storage_info]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    /// Bundles submitted successfully in current block.
    #[pallet::storage]
    pub(super) type SuccessfulBundles<T> = StorageMap<_, Identity, DomainId, Vec<H256>, ValueQuery>;

    /// Fraud proofs submitted successfully in current block.
    #[pallet::storage]
    pub(super) type SuccessfulFraudProofs<T: Config> =
        StorageMap<_, Identity, DomainId, Vec<T::DomainHash>, ValueQuery>;

    /// Stores the next runtime id.
    #[pallet::storage]
    pub(super) type NextRuntimeId<T> = StorageValue<_, RuntimeId, ValueQuery>;

    /// Starting EVM chain ID for evm runtimes.
    pub struct StartingEVMChainId;

    impl Get<EVMChainId> for StartingEVMChainId {
        fn get() -> EVMChainId {
            // after looking at `https://chainlist.org/?testnets=false`
            // we think starting with `490000` would not have much clashes
            490000
        }
    }

    /// Stores the next evm chain id.
    #[pallet::storage]
    pub(super) type NextEVMChainId<T> = StorageValue<_, EVMChainId, ValueQuery, StartingEVMChainId>;

    #[pallet::storage]
    pub(super) type RuntimeRegistry<T: Config> =
        StorageMap<_, Identity, RuntimeId, RuntimeObject<BlockNumberFor<T>, T::Hash>, OptionQuery>;

    #[pallet::storage]
    pub(super) type ScheduledRuntimeUpgrades<T: Config> = StorageDoubleMap<
        _,
        Identity,
        BlockNumberFor<T>,
        Identity,
        RuntimeId,
        ScheduledRuntimeUpgrade<T::Hash>,
        OptionQuery,
    >;

    #[pallet::storage]
    pub(super) type NextOperatorId<T> = StorageValue<_, OperatorId, ValueQuery>;

    #[pallet::storage]
    pub(super) type OperatorIdOwner<T: Config> =
        StorageMap<_, Identity, OperatorId, T::AccountId, OptionQuery>;

    /// Indexes operator signing key against OperatorId.
    #[pallet::storage]
    #[pallet::getter(fn operator_signing_key)]
    pub(super) type OperatorSigningKey<T: Config> =
        StorageMap<_, Identity, OperatorPublicKey, OperatorId, OptionQuery>;

    #[pallet::storage]
    #[pallet::getter(fn domain_staking_summary)]
    pub(super) type DomainStakingSummary<T: Config> =
        StorageMap<_, Identity, DomainId, StakingSummary<OperatorId, BalanceOf<T>>, OptionQuery>;

    /// List of all registered operators and their configuration.
    #[pallet::storage]
    pub(super) type Operators<T: Config> = StorageMap<
        _,
        Identity,
        OperatorId,
        Operator<BalanceOf<T>, T::Share, DomainBlockNumberFor<T>>,
        OptionQuery,
    >;

    /// Temporary hold of all the operators who decided to switch to another domain.
    /// Once epoch is complete, these operators are added to new domains under next_operators.
    #[pallet::storage]
    pub(super) type PendingOperatorSwitches<T: Config> =
        StorageMap<_, Identity, DomainId, BTreeSet<OperatorId>, OptionQuery>;

    /// Share price for the operator pool at the end of Domain epoch.
    // TODO: currently unbounded storage.
    #[pallet::storage]
    pub type OperatorEpochSharePrice<T: Config> =
        StorageDoubleMap<_, Identity, OperatorId, Identity, DomainEpoch, SharePrice, OptionQuery>;

    /// List of all deposits for given Operator.
    #[pallet::storage]
    pub(super) type Deposits<T: Config> = StorageDoubleMap<
        _,
        Identity,
        OperatorId,
        Identity,
        NominatorId<T>,
        Deposit<T::Share, BalanceOf<T>>,
        OptionQuery,
    >;

    /// List of all withdrawals for a given operator.
    #[pallet::storage]
    pub(super) type Withdrawals<T: Config> = StorageDoubleMap<
        _,
        Identity,
        OperatorId,
        Identity,
        NominatorId<T>,
        Withdrawal<BalanceOf<T>, T::Share, DomainBlockNumberFor<T>>,
        OptionQuery,
    >;

    /// Tracks the nominator count under given operator.
    /// This storage is necessary since CountedStorageNMap does not support prefix key count, so
    /// cannot use that storage type for `Nominators` storage.
    /// Note: The count is incremented for new nominators and decremented when the nominator withdraws
    /// all the stake.
    /// Since Operator themselves are first nominator, they are not counted.
    #[pallet::storage]
    pub(super) type NominatorCount<T: Config> =
        StorageMap<_, Identity, OperatorId, u32, ValueQuery>;

    /// A list operators who were slashed during the current epoch associated with the domain.
    /// When the epoch for a given domain is complete, operator total stake is moved to treasury and
    /// then deleted.
    #[pallet::storage]
    pub(super) type PendingSlashes<T: Config> =
        StorageMap<_, Identity, DomainId, BTreeSet<OperatorId>, OptionQuery>;

    /// The pending staking operation count of the current epoch, it should not larger than
    /// `MaxPendingStakingOperation` and will be resetted to 0 upon epoch transition.
    #[pallet::storage]
    pub(super) type PendingStakingOperationCount<T: Config> =
        StorageMap<_, Identity, DomainId, u32, ValueQuery>;

    /// Stores the next domain id.
    #[pallet::storage]
    pub(super) type NextDomainId<T> = StorageValue<_, DomainId, ValueQuery>;

    /// The domain registry
    #[pallet::storage]
    pub(super) type DomainRegistry<T: Config> = StorageMap<
        _,
        Identity,
        DomainId,
        DomainObject<BlockNumberFor<T>, ReceiptHashFor<T>, T::AccountId, BalanceOf<T>>,
        OptionQuery,
    >;

    /// The domain block tree, map (`domain_id`, `domain_block_number`) to the hash of ER,
    /// which can be used get the block tree node in `BlockTreeNodes`
    #[pallet::storage]
    pub(super) type BlockTree<T: Config> = StorageDoubleMap<
        _,
        Identity,
        DomainId,
        Identity,
        DomainBlockNumberFor<T>,
        ReceiptHashFor<T>,
        OptionQuery,
    >;

    /// Mapping of block tree node hash to the node, each node represent a domain block
    #[pallet::storage]
    pub(super) type BlockTreeNodes<T: Config> =
        StorageMap<_, Identity, ReceiptHashFor<T>, BlockTreeNodeFor<T>, OptionQuery>;

    /// The head receipt number of each domain
    #[pallet::storage]
    pub(super) type HeadReceiptNumber<T: Config> =
        StorageMap<_, Identity, DomainId, DomainBlockNumberFor<T>, ValueQuery>;

    /// Whether the head receipt have extended in the current consensus block
    ///
    /// Temporary storage only exist during block execution
    #[pallet::storage]
    pub(super) type HeadReceiptExtended<T: Config> =
        StorageMap<_, Identity, DomainId, bool, ValueQuery>;

    /// The consensus block hash used to verify ER,
    /// only store the consensus block hash for a domain
    /// if that consensus block contains bundle of the domain, the hash will be pruned when the ER
    /// that point to the consensus block is pruned.
    ///
    /// TODO: this storage is unbounded in some cases, see https://github.com/subspace/subspace/issues/1673
    /// for more details, this will be fixed once https://github.com/subspace/subspace/issues/1731 is implemented.
    #[pallet::storage]
    #[pallet::getter(fn consensus_block_info)]
    pub type ConsensusBlockHash<T: Config> =
        StorageDoubleMap<_, Identity, DomainId, Identity, BlockNumberFor<T>, T::Hash, OptionQuery>;

    /// A set of `BundleDigest` from all bundles that successfully submitted to the consensus block,
    /// these bundles will be used to construct the domain block and `ExecutionInbox` is used to:
    ///
    /// 1. Ensure subsequent ERs of that domain block include all pre-validated extrinsic bundles
    /// 2. Index the `InboxedBundleAuthor` and pruned its value when the corresponding `ExecutionInbox` is pruned
    #[pallet::storage]
    pub type ExecutionInbox<T: Config> = StorageNMap<
        _,
        (
            NMapKey<Identity, DomainId>,
            NMapKey<Identity, DomainBlockNumberFor<T>>,
            NMapKey<Identity, BlockNumberFor<T>>,
        ),
        Vec<BundleDigest<T::DomainHash>>,
        ValueQuery,
    >;

    /// A mapping of `bundle_header_hash` -> `bundle_author` for all the successfully submitted bundles of
    /// the last `BlockTreePruningDepth` domain blocks. Used to verify the invalid bundle fraud proof and
    /// slash malicious operator who have submitted invalid bundle.
    #[pallet::storage]
    pub(super) type InboxedBundleAuthor<T: Config> =
        StorageMap<_, Identity, T::DomainHash, OperatorId, OptionQuery>;

    /// The block number of the best domain block, increase by one when the first bundle of the domain is
    /// successfully submitted to current consensus block, which mean a new domain block with this block
    /// number will be produce. Used as a pointer in `ExecutionInbox` to identify the current under building
    /// domain block, also used as a mapping of consensus block number to domain block number.
    #[pallet::storage]
    pub(super) type HeadDomainNumber<T: Config> =
        StorageMap<_, Identity, DomainId, DomainBlockNumberFor<T>, ValueQuery>;

    /// A temporary storage to hold any previous epoch details for a given domain
    /// if the epoch transitioned in this block so that all the submitted bundles
    /// within this block are verified.
    /// TODO: The storage is cleared on block finalization that means this storage is already cleared when
    /// verifying the `submit_bundle` extrinsic and not used at all
    #[pallet::storage]
    pub(super) type LastEpochStakingDistribution<T: Config> =
        StorageMap<_, Identity, DomainId, ElectionVerificationParams<BalanceOf<T>>, OptionQuery>;

    /// Storage to hold all the domain's latest confirmed block.
    #[pallet::storage]
    pub(super) type LatestConfirmedDomainBlock<T: Config> = StorageMap<
        _,
        Identity,
        DomainId,
        ConfirmedDomainBlock<DomainBlockNumberFor<T>, T::DomainHash>,
        OptionQuery,
    >;

    /// The latest ER submitted by the operator for a given domain. It is used to determine if the operator
    /// has submitted bad ER and is pending to slash.
    ///
    /// The storage item of a given `(domain_id, operator_id)` will be pruned after either:
    /// - All the ERs submitted by the operator for this domain are confirmed and pruned
    /// - All the bad ERs submitted by the operator for this domain are pruned and the operator is slashed
    #[pallet::storage]
    #[pallet::getter(fn latest_submitted_er)]
    pub(super) type LatestSubmittedER<T: Config> =
        StorageMap<_, Identity, (DomainId, OperatorId), DomainBlockNumberFor<T>, ValueQuery>;

    /// Storage for PermissionedActions for domain instantiation and other permissioned calls.
    #[pallet::storage]
    pub(super) type PermissionedActionAllowedBy<T: Config> =
        StorageValue<_, sp_domains::PermissionedActionAllowedBy<T::AccountId>, OptionQuery>;

    #[derive(TypeInfo, Encode, Decode, PalletError, Debug, PartialEq)]
    pub enum BundleError {
        /// Can not find the operator for given operator id.
        InvalidOperatorId,
        /// Invalid signature on the bundle header.
        BadBundleSignature,
        /// Invalid vrf signature in the proof of election.
        BadVrfSignature,
        /// Can not find the domain for given domain id.
        InvalidDomainId,
        /// Operator is not allowed to produce bundles in current epoch.
        BadOperator,
        /// Failed to pass the threshold check.
        ThresholdUnsatisfied,
        /// The Bundle is created too long ago.
        StaleBundle,
        /// An invalid execution receipt found in the bundle.
        Receipt(BlockTreeError),
        /// Bundle size exceed the max bundle size limit in the domain config
        BundleTooLarge,
        /// Bundle with an invalid extrinsic root
        InvalidExtrinsicRoot,
        /// This bundle duplicated with an already submitted bundle
        DuplicatedBundle,
        /// Invalid proof of time in the proof of election
        InvalidProofOfTime,
        /// The bundle is built on a slot in the future
        SlotInTheFuture,
        /// The bundle is built on a slot in the past
        SlotInThePast,
        /// Unable to calculate bundle limit
        UnableToCalculateBundleLimit,
        /// Bundle weight exceeds the max bundle weight limit
        BundleTooHeavy,
    }

    #[derive(TypeInfo, Encode, Decode, PalletError, Debug, PartialEq)]
    pub enum FraudProofError {
        /// The targeted bad receipt not found which may already pruned by other
        /// fraud proof or the fraud proof is submitted to the wrong fork.
        BadReceiptNotFound,
        /// The genesis receipt is unchallengeable.
        ChallengingGenesisReceipt,
        /// The descendants of the fraudulent ER is not pruned
        DescendantsOfFraudulentERNotPruned,
        /// Invalid fraud proof since block fees are not mismatched.
        InvalidBlockFeesFraudProof,
        /// Invalid fraud proof since transfers are not mismatched.
        InvalidTransfersFraudProof,
        /// Invalid domain block hash fraud proof.
        InvalidDomainBlockHashFraudProof,
        /// Invalid domain extrinsic fraud proof
        InvalidExtrinsicRootFraudProof,
        /// Invalid state transition fraud proof
        InvalidStateTransitionFraudProof,
        /// Parent receipt not found.
        ParentReceiptNotFound,
        /// Invalid bundles fraud proof
        InvalidBundleFraudProof,
        /// Bad/Invalid valid bundle fraud proof
        BadValidBundleFraudProof,
        /// Missing operator.
        MissingOperator,
        /// Unexpected fraud proof.
        UnexpectedFraudProof,
        /// Bad/Invalid bundle equivocation fraud proof.
        BadBundleEquivocationFraudProof,
        /// The bad receipt already reported by a previous fraud proof
        BadReceiptAlreadyReported,
    }

    impl<T> From<FraudProofError> for Error<T> {
        fn from(err: FraudProofError) -> Self {
            Error::FraudProof(err)
        }
    }

    impl<T> From<RuntimeRegistryError> for Error<T> {
        fn from(err: RuntimeRegistryError) -> Self {
            Error::RuntimeRegistry(err)
        }
    }

    impl<T> From<StakingError> for Error<T> {
        fn from(err: StakingError) -> Self {
            Error::Staking(err)
        }
    }

    impl<T> From<StakingEpochError> for Error<T> {
        fn from(err: StakingEpochError) -> Self {
            Error::StakingEpoch(err)
        }
    }

    impl<T> From<DomainRegistryError> for Error<T> {
        fn from(err: DomainRegistryError) -> Self {
            Error::DomainRegistry(err)
        }
    }

    impl<T> From<BlockTreeError> for Error<T> {
        fn from(err: BlockTreeError) -> Self {
            Error::BlockTree(err)
        }
    }

    impl From<ProofOfElectionError> for BundleError {
        fn from(err: ProofOfElectionError) -> Self {
            match err {
                ProofOfElectionError::BadVrfProof => Self::BadVrfSignature,
                ProofOfElectionError::ThresholdUnsatisfied => Self::ThresholdUnsatisfied,
            }
        }
    }

    impl<T> From<BundleStorageFundError> for Error<T> {
        fn from(err: BundleStorageFundError) -> Self {
            Error::BundleStorageFund(err)
        }
    }

    #[pallet::error]
    pub enum Error<T> {
        /// Invalid fraud proof.
        FraudProof(FraudProofError),
        /// Runtime registry specific errors
        RuntimeRegistry(RuntimeRegistryError),
        /// Staking related errors.
        Staking(StakingError),
        /// Staking epoch specific errors.
        StakingEpoch(StakingEpochError),
        /// Domain registry specific errors
        DomainRegistry(DomainRegistryError),
        /// Block tree specific errors
        BlockTree(BlockTreeError),
        /// Bundle storage fund specific errors
        BundleStorageFund(BundleStorageFundError),
        /// Permissioned action is not allowed by the caller.
        PermissionedActionNotAllowed,
    }

    /// Reason for slashing an operator
    #[derive(Clone, Debug, PartialEq, Encode, Decode, TypeInfo)]
    pub enum SlashedReason<DomainBlock, ReceiptHash> {
        /// Operator produced bad bundle.
        InvalidBundle(DomainBlock),
        /// Operator submitted bad Execution receipt.
        BadExecutionReceipt(ReceiptHash),
        /// Operator caused Bundle equivocation
        BundleEquivocation(Slot),
    }

    #[pallet::event]
    #[pallet::generate_deposit(pub (super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// A domain bundle was included.
        BundleStored {
            domain_id: DomainId,
            bundle_hash: H256,
            bundle_author: OperatorId,
        },
        DomainRuntimeCreated {
            runtime_id: RuntimeId,
            runtime_type: RuntimeType,
        },
        DomainRuntimeUpgradeScheduled {
            runtime_id: RuntimeId,
            scheduled_at: BlockNumberFor<T>,
        },
        DomainRuntimeUpgraded {
            runtime_id: RuntimeId,
        },
        OperatorRegistered {
            operator_id: OperatorId,
            domain_id: DomainId,
        },
        OperatorNominated {
            operator_id: OperatorId,
            nominator_id: NominatorId<T>,
        },
        DomainInstantiated {
            domain_id: DomainId,
        },
        OperatorSwitchedDomain {
            old_domain_id: DomainId,
            new_domain_id: DomainId,
        },
        OperatorDeregistered {
            operator_id: OperatorId,
        },
        OperatorUnlocked {
            operator_id: OperatorId,
        },
        WithdrewStake {
            operator_id: OperatorId,
            nominator_id: NominatorId<T>,
        },
        FundsUnlocked {
            operator_id: OperatorId,
            nominator_id: NominatorId<T>,
            amount: BalanceOf<T>,
        },
        PreferredOperator {
            operator_id: OperatorId,
            nominator_id: NominatorId<T>,
        },
        OperatorRewarded {
            operator_id: OperatorId,
            reward: BalanceOf<T>,
        },
        OperatorTaxCollected {
            operator_id: OperatorId,
            tax: BalanceOf<T>,
        },
        DomainEpochCompleted {
            domain_id: DomainId,
            completed_epoch_index: EpochIndex,
        },
        ForceDomainEpochTransition {
            domain_id: DomainId,
            completed_epoch_index: EpochIndex,
        },
        FraudProofProcessed {
            domain_id: DomainId,
            new_head_receipt_number: Option<DomainBlockNumberFor<T>>,
        },
        DomainOperatorAllowListUpdated {
            domain_id: DomainId,
        },
        OperatorSlashed {
            operator_id: OperatorId,
            reason: SlashedReason<DomainBlockNumberFor<T>, ReceiptHashFor<T>>,
        },
        StorageFeeDeposited {
            operator_id: OperatorId,
            nominator_id: NominatorId<T>,
            amount: BalanceOf<T>,
        },
    }

    /// Per-domain state for tx range calculation.
    #[derive(Debug, Default, Decode, Encode, TypeInfo, PartialEq, Eq)]
    pub struct TxRangeState {
        /// Current tx range.
        pub tx_range: U256,

        /// Blocks in the current adjustment interval.
        pub interval_blocks: u64,

        /// Bundles in the current adjustment interval.
        pub interval_bundles: u64,
    }

    impl TxRangeState {
        /// Called when a bundle is added to the current block.
        pub fn on_bundle(&mut self) {
            self.interval_bundles += 1;
        }
    }

    #[pallet::storage]
    pub(super) type DomainTxRangeState<T: Config> =
        StorageMap<_, Identity, DomainId, TxRangeState, OptionQuery>;

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        #[pallet::call_index(0)]
        #[pallet::weight(Pallet::<T>::max_submit_bundle_weight())]
        pub fn submit_bundle(
            origin: OriginFor<T>,
            opaque_bundle: OpaqueBundleOf<T>,
        ) -> DispatchResultWithPostInfo {
            ensure_none(origin)?;

            log::trace!(target: "runtime::domains", "Processing bundle: {opaque_bundle:?}");

            let domain_id = opaque_bundle.domain_id();
            let bundle_hash = opaque_bundle.hash();
            let bundle_header_hash = opaque_bundle.sealed_header.pre_hash();
            let extrinsics_root = opaque_bundle.extrinsics_root();
            let operator_id = opaque_bundle.operator_id();
            let bundle_size = opaque_bundle.size();
            let receipt = opaque_bundle.into_receipt();
            #[cfg_attr(feature = "runtime-benchmarks", allow(unused_variables))]
            let receipt_block_number = receipt.domain_block_number;

            #[cfg(not(feature = "runtime-benchmarks"))]
            let mut actual_weight = T::WeightInfo::submit_bundle();
            #[cfg(feature = "runtime-benchmarks")]
            let actual_weight = T::WeightInfo::submit_bundle();

            match execution_receipt_type::<T>(domain_id, &receipt) {
                ReceiptType::Rejected(rejected_receipt_type) => {
                    return Err(Error::<T>::BlockTree(rejected_receipt_type.into()).into());
                }
                // Add the exeuctione receipt to the block tree
                ReceiptType::Accepted(accepted_receipt_type) => {
                    // Before adding the new head receipt to the block tree, try to prune any previous
                    // bad ER at the same domain block and slash the submitter.
                    //
                    // NOTE: Skip the following staking related operations when benchmarking the
                    // `submit_bundle` call, these operations will be benchmarked separately.
                    #[cfg(not(feature = "runtime-benchmarks"))]
                    if accepted_receipt_type == AcceptedReceiptType::NewHead {
                        if let Some(block_tree_node) =
                            prune_receipt::<T>(domain_id, receipt_block_number)
                                .map_err(Error::<T>::from)?
                        {
                            actual_weight =
                                actual_weight.saturating_add(T::WeightInfo::handle_bad_receipt(
                                    block_tree_node.operator_ids.len() as u32,
                                ));

                            let bad_receipt_hash = block_tree_node
                                .execution_receipt
                                .hash::<DomainHashingFor<T>>();
                            do_slash_operators::<T>(
                                block_tree_node.operator_ids.into_iter(),
                                SlashedReason::BadExecutionReceipt(bad_receipt_hash),
                            )
                            .map_err(Error::<T>::from)?;
                        }
                    }

                    #[cfg_attr(feature = "runtime-benchmarks", allow(unused_variables))]
                    let maybe_confirmed_domain_block_info = process_execution_receipt::<T>(
                        domain_id,
                        operator_id,
                        receipt,
                        accepted_receipt_type,
                    )
                    .map_err(Error::<T>::from)?;

                    // If any domain block is confirmed, then we have a new head added
                    // so distribute the operator rewards and, if required, do epoch transition as well.
                    //
                    // NOTE: Skip the following staking related operations when benchmarking the
                    // `submit_bundle` call, these operations will be benchmarked separately.
                    #[cfg(not(feature = "runtime-benchmarks"))]
                    if let Some(confirmed_block_info) = maybe_confirmed_domain_block_info {
                        actual_weight =
                            actual_weight.saturating_add(T::WeightInfo::confirm_domain_block(
                                confirmed_block_info.operator_ids.len() as u32,
                                confirmed_block_info.invalid_bundle_authors.len() as u32,
                            ));

                        refund_storage_fee::<T>(
                            confirmed_block_info.total_storage_fee,
                            confirmed_block_info.paid_bundle_storage_fees,
                        )
                        .map_err(Error::<T>::from)?;

                        do_reward_operators::<T>(
                            domain_id,
                            confirmed_block_info.operator_ids.into_iter(),
                            confirmed_block_info.rewards,
                        )
                        .map_err(Error::<T>::from)?;

                        do_slash_operators::<T>(
                            confirmed_block_info.invalid_bundle_authors.into_iter(),
                            SlashedReason::InvalidBundle(confirmed_block_info.domain_block_number),
                        )
                        .map_err(Error::<T>::from)?;

                        if confirmed_block_info.domain_block_number % T::StakeEpochDuration::get()
                            == Zero::zero()
                        {
                            let epoch_transition_res =
                                do_finalize_domain_current_epoch::<T>(domain_id)
                                    .map_err(Error::<T>::from)?;

                            Self::deposit_event(Event::DomainEpochCompleted {
                                domain_id,
                                completed_epoch_index: epoch_transition_res.completed_epoch_index,
                            });

                            actual_weight = actual_weight.saturating_add(
                                Self::actual_epoch_transition_weight(epoch_transition_res),
                            );
                        }
                    }
                }
            }

            // `SuccessfulBundles` is empty means this is the first accepted bundle for this domain in this
            // consensus block, which also mean a domain block will be produced thus update `HeadDomainNumber`
            // to this domain block's block number.
            if SuccessfulBundles::<T>::get(domain_id).is_empty() {
                let next_number = HeadDomainNumber::<T>::get(domain_id)
                    .checked_add(&One::one())
                    .ok_or::<Error<T>>(BlockTreeError::MaxHeadDomainNumber.into())?;
                HeadDomainNumber::<T>::set(domain_id, next_number);
            }

            // Put the `extrinsics_root` to the inbox of the current under building domain block
            let head_domain_number = HeadDomainNumber::<T>::get(domain_id);
            let consensus_block_number = frame_system::Pallet::<T>::current_block_number();
            ExecutionInbox::<T>::append(
                (domain_id, head_domain_number, consensus_block_number),
                BundleDigest {
                    header_hash: bundle_header_hash,
                    extrinsics_root,
                    size: bundle_size,
                },
            );

            InboxedBundleAuthor::<T>::insert(bundle_header_hash, operator_id);

            SuccessfulBundles::<T>::append(domain_id, bundle_hash);

            Self::deposit_event(Event::BundleStored {
                domain_id,
                bundle_hash,
                bundle_author: operator_id,
            });

            // Ensure the returned weight not exceed the maximum weight in the `pallet::weight`
            Ok(Some(actual_weight.min(Self::max_submit_bundle_weight())).into())
        }

        #[pallet::call_index(1)]
        #[pallet::weight((
            T::WeightInfo::submit_fraud_proof().saturating_add(
                T::WeightInfo::handle_bad_receipt(MAX_BUNLDE_PER_BLOCK)
            ),
            DispatchClass::Operational,
            Pays::No
        ))]
        pub fn submit_fraud_proof(
            origin: OriginFor<T>,
            fraud_proof: Box<FraudProof<BlockNumberFor<T>, T::Hash, T::DomainHeader>>,
        ) -> DispatchResultWithPostInfo {
            ensure_none(origin)?;

            log::trace!(target: "runtime::domains", "Processing fraud proof: {fraud_proof:?}");
            let domain_id = fraud_proof.domain_id();
            let mut actual_weight = T::WeightInfo::submit_fraud_proof();

            if let Some(bad_receipt_hash) = fraud_proof.targeted_bad_receipt_hash() {
                let head_receipt_number = HeadReceiptNumber::<T>::get(domain_id);
                let bad_receipt_number = BlockTreeNodes::<T>::get(bad_receipt_hash)
                    .ok_or::<Error<T>>(FraudProofError::BadReceiptNotFound.into())?
                    .execution_receipt
                    .domain_block_number;
                // The `head_receipt_number` must greater than or equal to any existing receipt, including
                // the bad receipt, otherwise the fraud proof should be rejected due to `BadReceiptNotFound`,
                // double check here to make it more robust.
                ensure!(
                    head_receipt_number >= bad_receipt_number,
                    Error::<T>::from(FraudProofError::BadReceiptNotFound),
                );

                // Prune the bad ER and slash the submitter, the descendants of the bad ER (i.e. all ERs in
                // `[bad_receipt_number + 1..head_receipt_number]` ) and the corresponding submitter will be
                // pruned/slashed lazily as the domain progressed.
                //
                // NOTE: Skip the following staking related operations when benchmarking the
                // `submit_fraud_proof` call, these operations will be benchmarked separately.
                #[cfg(not(feature = "runtime-benchmarks"))]
                {
                    let block_tree_node = prune_receipt::<T>(domain_id, bad_receipt_number)
                        .map_err(Error::<T>::from)?
                        .ok_or::<Error<T>>(FraudProofError::BadReceiptNotFound.into())?;

                    actual_weight =
                        actual_weight.saturating_add(T::WeightInfo::handle_bad_receipt(
                            (block_tree_node.operator_ids.len() as u32).min(MAX_BUNLDE_PER_BLOCK),
                        ));

                    do_slash_operators::<T>(
                        block_tree_node.operator_ids.into_iter(),
                        SlashedReason::BadExecutionReceipt(bad_receipt_hash),
                    )
                    .map_err(Error::<T>::from)?;
                }

                // Update the head receipt number to `bad_receipt_number - 1`
                let new_head_receipt_number = bad_receipt_number.saturating_sub(One::one());
                HeadReceiptNumber::<T>::insert(domain_id, new_head_receipt_number);

                Self::deposit_event(Event::FraudProofProcessed {
                    domain_id,
                    new_head_receipt_number: Some(new_head_receipt_number),
                });
            } else if let Some((targeted_bad_operator, slot)) =
                fraud_proof.targeted_bad_operator_and_slot_for_bundle_equivocation()
            {
                Self::deposit_event(Event::FraudProofProcessed {
                    domain_id,
                    new_head_receipt_number: None,
                });

                do_slash_operators::<T>(
                    vec![targeted_bad_operator].into_iter(),
                    SlashedReason::BundleEquivocation(slot),
                )
                .map_err(Error::<T>::from)?;

                actual_weight = actual_weight.saturating_add(T::WeightInfo::handle_bad_receipt(1));
            }

            SuccessfulFraudProofs::<T>::append(domain_id, fraud_proof.hash());

            Ok(Some(actual_weight).into())
        }

        #[pallet::call_index(2)]
        #[pallet::weight(T::WeightInfo::register_domain_runtime())]
        pub fn register_domain_runtime(
            origin: OriginFor<T>,
            runtime_name: String,
            runtime_type: RuntimeType,
            // TODO: we can use `RawGenesis` as argument directly to avoid decoding but the in tool like
            // `polkadot.js` it will required the user to provide each field of the struct type and not
            // support upload file which will brings bad UX.
            raw_genesis_storage: Vec<u8>,
        ) -> DispatchResult {
            ensure_root(origin)?;

            let block_number = frame_system::Pallet::<T>::current_block_number();
            let runtime_id = do_register_runtime::<T>(
                runtime_name,
                runtime_type.clone(),
                raw_genesis_storage,
                block_number,
            )
            .map_err(Error::<T>::from)?;

            Self::deposit_event(Event::DomainRuntimeCreated {
                runtime_id,
                runtime_type,
            });

            Ok(())
        }

        #[pallet::call_index(3)]
        #[pallet::weight(T::WeightInfo::upgrade_domain_runtime())]
        pub fn upgrade_domain_runtime(
            origin: OriginFor<T>,
            runtime_id: RuntimeId,
            raw_genesis_storage: Vec<u8>,
        ) -> DispatchResult {
            ensure_root(origin)?;

            let block_number = frame_system::Pallet::<T>::current_block_number();
            let scheduled_at =
                do_schedule_runtime_upgrade::<T>(runtime_id, raw_genesis_storage, block_number)
                    .map_err(Error::<T>::from)?;

            Self::deposit_event(Event::DomainRuntimeUpgradeScheduled {
                runtime_id,
                scheduled_at,
            });

            Ok(())
        }

        #[pallet::call_index(4)]
        #[pallet::weight(T::WeightInfo::register_operator())]
        pub fn register_operator(
            origin: OriginFor<T>,
            domain_id: DomainId,
            amount: BalanceOf<T>,
            config: OperatorConfig<BalanceOf<T>>,
        ) -> DispatchResult {
            let owner = ensure_signed(origin)?;

            let (operator_id, current_epoch_index) =
                do_register_operator::<T>(owner, domain_id, amount, config)
                    .map_err(Error::<T>::from)?;

            Self::deposit_event(Event::OperatorRegistered {
                operator_id,
                domain_id,
            });

            // if the domain's current epoch is 0,
            // then do an epoch transition so that operator can start producing bundles
            if current_epoch_index.is_zero() {
                do_finalize_domain_current_epoch::<T>(domain_id).map_err(Error::<T>::from)?;
            }

            Ok(())
        }

        #[pallet::call_index(5)]
        #[pallet::weight(T::WeightInfo::nominate_operator())]
        pub fn nominate_operator(
            origin: OriginFor<T>,
            operator_id: OperatorId,
            amount: BalanceOf<T>,
        ) -> DispatchResult {
            let nominator_id = ensure_signed(origin)?;

            do_nominate_operator::<T>(operator_id, nominator_id.clone(), amount)
                .map_err(Error::<T>::from)?;

            Self::deposit_event(Event::OperatorNominated {
                operator_id,
                nominator_id,
            });

            Ok(())
        }

        #[pallet::call_index(6)]
        #[pallet::weight(T::WeightInfo::instantiate_domain())]
        pub fn instantiate_domain(
            origin: OriginFor<T>,
            domain_config: DomainConfig<T::AccountId, BalanceOf<T>>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(
                PermissionedActionAllowedBy::<T>::get()
                    .map(|allowed_by| allowed_by.is_allowed(&who))
                    .unwrap_or_default(),
                Error::<T>::PermissionedActionNotAllowed
            );

            let created_at = frame_system::Pallet::<T>::current_block_number();

            let domain_id = do_instantiate_domain::<T>(domain_config, who, created_at)
                .map_err(Error::<T>::from)?;

            Self::deposit_event(Event::DomainInstantiated { domain_id });

            Ok(())
        }

        #[pallet::call_index(8)]
        #[pallet::weight(T::WeightInfo::deregister_operator())]
        pub fn deregister_operator(
            origin: OriginFor<T>,
            operator_id: OperatorId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            do_deregister_operator::<T>(who, operator_id).map_err(Error::<T>::from)?;

            Self::deposit_event(Event::OperatorDeregistered { operator_id });

            Ok(())
        }

        #[pallet::call_index(9)]
        #[pallet::weight(T::WeightInfo::withdraw_stake())]
        pub fn withdraw_stake(
            origin: OriginFor<T>,
            operator_id: OperatorId,
            shares: T::Share,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            do_withdraw_stake::<T>(operator_id, who.clone(), shares).map_err(Error::<T>::from)?;

            Self::deposit_event(Event::WithdrewStake {
                operator_id,
                nominator_id: who,
            });

            Ok(())
        }

        /// Unlocks the first withdrawal given the unlocking period is complete.
        /// Even if rest of the withdrawals are out of unlocking period, nominator
        /// should call this extrinsic to unlock each withdrawal
        #[pallet::call_index(10)]
        #[pallet::weight(T::WeightInfo::unlock_funds())]
        pub fn unlock_funds(origin: OriginFor<T>, operator_id: OperatorId) -> DispatchResult {
            let nominator_id = ensure_signed(origin)?;
            let unlocked_funds = do_unlock_funds::<T>(operator_id, nominator_id.clone())
                .map_err(crate::pallet::Error::<T>::from)?;
            Self::deposit_event(Event::FundsUnlocked {
                operator_id,
                nominator_id,
                amount: unlocked_funds,
            });
            Ok(())
        }

        /// Unlocks the operator given the unlocking period is complete.
        /// Anyone can initiate the operator unlock.
        #[pallet::call_index(11)]
        #[pallet::weight(T::WeightInfo::unlock_operator(T::MaxNominators::get()))]
        pub fn unlock_operator(
            origin: OriginFor<T>,
            operator_id: OperatorId,
        ) -> DispatchResultWithPostInfo {
            ensure_signed(origin)?;

            let nominator_count =
                do_unlock_operator::<T>(operator_id).map_err(crate::pallet::Error::<T>::from)?;

            Self::deposit_event(Event::OperatorUnlocked { operator_id });

            Ok(Some(T::WeightInfo::unlock_operator(
                (nominator_count as u32).min(T::MaxNominators::get()),
            ))
            .into())
        }

        /// Extrinsic to update domain's operator allow list.
        /// Note:
        /// - If the previous allowed list is set to specific operators and new allow list is set
        ///   to `Anyone`, then domain will become permissioned to open for all operators.
        /// - If the previous allowed list is set to `Anyone` or specific operators and the new
        ///   allow list is set to specific operators, then all the registered not allowed operators
        ///   will continue to operate until they de-register themselves.
        #[pallet::call_index(12)]
        #[pallet::weight(T::WeightInfo::update_domain_operator_allow_list())]
        pub fn update_domain_operator_allow_list(
            origin: OriginFor<T>,
            domain_id: DomainId,
            operator_allow_list: OperatorAllowList<T::AccountId>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            do_update_domain_allow_list::<T>(who, domain_id, operator_allow_list)
                .map_err(Error::<T>::from)?;
            Self::deposit_event(crate::pallet::Event::DomainOperatorAllowListUpdated { domain_id });
            Ok(())
        }

        /// Force staking epoch transition for a given domain
        #[pallet::call_index(13)]
        #[pallet::weight(Pallet::<T>::max_staking_epoch_transition())]
        pub fn force_staking_epoch_transition(
            origin: OriginFor<T>,
            domain_id: DomainId,
        ) -> DispatchResultWithPostInfo {
            ensure_root(origin)?;

            let epoch_transition_res =
                do_finalize_domain_current_epoch::<T>(domain_id).map_err(Error::<T>::from)?;

            Self::deposit_event(Event::ForceDomainEpochTransition {
                domain_id,
                completed_epoch_index: epoch_transition_res.completed_epoch_index,
            });

            // Ensure the returned weight not exceed the maximum weight in the `pallet::weight`
            let actual_weight = Self::actual_epoch_transition_weight(epoch_transition_res)
                .min(Self::max_staking_epoch_transition());

            Ok(Some(actual_weight).into())
        }

        /// Update permissioned action allowed by storage by Sudo.
        #[pallet::call_index(14)]
        #[pallet::weight(<T as frame_system::Config>::DbWeight::get().reads_writes(0, 1))]
        pub fn set_permissioned_action_allowed_by(
            origin: OriginFor<T>,
            permissioned_action_allowed_by: sp_domains::PermissionedActionAllowedBy<T::AccountId>,
        ) -> DispatchResult {
            ensure_root(origin)?;
            PermissionedActionAllowedBy::<T>::put(permissioned_action_allowed_by);
            Ok(())
        }
    }

    #[pallet::genesis_config]
    pub struct GenesisConfig<T: Config> {
        pub genesis_domain: Option<GenesisDomain<T::AccountId, BalanceOf<T>>>,
        pub permissioned_action_allowed_by:
            Option<sp_domains::PermissionedActionAllowedBy<T::AccountId>>,
    }

    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            GenesisConfig {
                genesis_domain: None,
                permissioned_action_allowed_by: None,
            }
        }
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            if let Some(permissioned_action_allowed_by) =
                self.permissioned_action_allowed_by.as_ref().cloned()
            {
                PermissionedActionAllowedBy::<T>::put(permissioned_action_allowed_by)
            }
            if let Some(genesis_domain) = self.genesis_domain.as_ref().cloned() {
                // Register the genesis domain runtime
                let runtime_id = register_runtime_at_genesis::<T>(
                    genesis_domain.runtime_name,
                    genesis_domain.runtime_type,
                    genesis_domain.runtime_version,
                    genesis_domain.raw_genesis_storage,
                    Zero::zero(),
                )
                .expect("Genesis runtime registration must always succeed");

                // Instantiate the genesis domain
                let domain_config = DomainConfig {
                    domain_name: genesis_domain.domain_name,
                    runtime_id,
                    max_block_size: genesis_domain.max_block_size,
                    max_block_weight: genesis_domain.max_block_weight,
                    bundle_slot_probability: genesis_domain.bundle_slot_probability,
                    target_bundles_per_block: genesis_domain.target_bundles_per_block,
                    operator_allow_list: genesis_domain.operator_allow_list,
                    initial_balances: genesis_domain.initial_balances,
                };
                let domain_owner = genesis_domain.owner_account_id;
                let domain_id =
                    do_instantiate_domain::<T>(domain_config, domain_owner.clone(), Zero::zero())
                        .expect("Genesis domain instantiation must always succeed");

                // Register domain_owner as the genesis operator.
                let operator_config = OperatorConfig {
                    signing_key: genesis_domain.signing_key.clone(),
                    minimum_nominator_stake: genesis_domain.minimum_nominator_stake,
                    nomination_tax: genesis_domain.nomination_tax,
                };
                let operator_stake = T::MinOperatorStake::get();
                do_register_operator::<T>(domain_owner, domain_id, operator_stake, operator_config)
                    .expect("Genesis operator registration must succeed");

                do_finalize_domain_current_epoch::<T>(domain_id)
                    .expect("Genesis epoch must succeed");
            }
        }
    }

    #[pallet::hooks]
    // TODO: proper benchmark
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        fn on_initialize(block_number: BlockNumberFor<T>) -> Weight {
            // Do scheduled domain runtime upgrade
            do_upgrade_runtimes::<T>(block_number);

            // Store the hash of the parent consensus block for domain that have bundles submitted
            // in that consensus block
            let parent_number = block_number - One::one();
            let parent_hash = frame_system::Pallet::<T>::block_hash(parent_number);
            for (domain_id, _) in SuccessfulBundles::<T>::drain() {
                ConsensusBlockHash::<T>::insert(domain_id, parent_number, parent_hash);
                T::DomainBundleSubmitted::domain_bundle_submitted(domain_id);
            }

            let _ = SuccessfulFraudProofs::<T>::clear(u32::MAX, None);

            Weight::zero()
        }

        fn on_finalize(_: BlockNumberFor<T>) {
            let _ = LastEpochStakingDistribution::<T>::clear(u32::MAX, None);
            let _ = HeadReceiptExtended::<T>::clear(u32::MAX, None);
        }
    }

    #[pallet::validate_unsigned]
    impl<T: Config> ValidateUnsigned for Pallet<T> {
        type Call = Call<T>;
        fn pre_dispatch(call: &Self::Call) -> Result<(), TransactionValidityError> {
            match call {
                Call::submit_bundle { opaque_bundle } => Self::validate_bundle(opaque_bundle, true)
                    .map_err(|_| InvalidTransaction::Call.into())
                    .and_then(|_| {
                        charge_bundle_storage_fee::<T>(
                            opaque_bundle.operator_id(),
                            opaque_bundle.size(),
                        )
                        .map_err(|_| InvalidTransaction::Call.into())
                    }),
                Call::submit_fraud_proof { fraud_proof } => Self::validate_fraud_proof(fraud_proof)
                    .map(|_| ())
                    .map_err(|_| InvalidTransaction::Call.into()),
                _ => Err(InvalidTransaction::Call.into()),
            }
        }

        fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
            match call {
                Call::submit_bundle { opaque_bundle } => {
                    if let Err(e) = Self::validate_bundle(opaque_bundle, false) {
                        match e {
                            // These errors are common due to networking delay or chain re-org,
                            // using a lower log level to avoid the noise.
                            BundleError::Receipt(BlockTreeError::InFutureReceipt)
                            | BundleError::Receipt(BlockTreeError::StaleReceipt)
                            | BundleError::Receipt(BlockTreeError::NewBranchReceipt)
                            | BundleError::Receipt(BlockTreeError::UnavailableConsensusBlockHash)
                            | BundleError::Receipt(BlockTreeError::BuiltOnUnknownConsensusBlock)
                            | BundleError::DuplicatedBundle
                            | BundleError::SlotInThePast
                            | BundleError::SlotInTheFuture
                            | BundleError::InvalidProofOfTime => {
                                log::debug!(
                                    target: "runtime::domains",
                                    "Bad bundle {:?}, error: {e:?}", opaque_bundle.domain_id(),
                                );
                            }
                            _ => {
                                log::warn!(
                                    target: "runtime::domains",
                                    "Bad bundle {:?}, operator {}, error: {e:?}",
                                    opaque_bundle.domain_id(),
                                    opaque_bundle.operator_id(),
                                );
                            }
                        }
                        if let BundleError::Receipt(_) = e {
                            return InvalidTransactionCode::ExecutionReceipt.into();
                        } else {
                            return InvalidTransactionCode::Bundle.into();
                        }
                    }

                    if let Err(e) = charge_bundle_storage_fee::<T>(
                        opaque_bundle.operator_id(),
                        opaque_bundle.size(),
                    ) {
                        log::debug!(
                            target: "runtime::domains",
                            "Operator {} unable to pay for the bundle storage fee, domain id {:?}, error: {e:?}",
                            opaque_bundle.operator_id(),
                            opaque_bundle.domain_id(),
                        );
                        return InvalidTransactionCode::BundleStorageFeePayment.into();
                    }

                    ValidTransaction::with_tag_prefix("SubspaceSubmitBundle")
                        // Bundle have a bit higher priority than normal extrinsic but must less than
                        // fraud proof
                        .priority(1)
                        .longevity(T::ConfirmationDepthK::get().try_into().unwrap_or_else(|_| {
                            panic!("Block number always fits in TransactionLongevity; qed")
                        }))
                        .and_provides(opaque_bundle.hash())
                        .propagate(true)
                        .build()
                }
                Call::submit_fraud_proof { fraud_proof } => {
                    let (tag, priority) = match Self::validate_fraud_proof(fraud_proof) {
                        Err(e) => {
                            log::warn!(
                                target: "runtime::domains",
                                "Bad fraud proof {fraud_proof:?}, error: {e:?}",
                            );
                            return InvalidTransactionCode::FraudProof.into();
                        }
                        Ok(tp) => tp,
                    };

                    ValidTransaction::with_tag_prefix("SubspaceSubmitFraudProof")
                        .priority(priority)
                        .and_provides(tag)
                        .longevity(TransactionLongevity::MAX)
                        // We need this extrinsic to be propagated to the farmer nodes.
                        .propagate(true)
                        .build()
                }

                _ => InvalidTransaction::Call.into(),
            }
        }
    }
}

impl<T: Config> Pallet<T> {
    pub fn successful_bundles(domain_id: DomainId) -> Vec<H256> {
        SuccessfulBundles::<T>::get(domain_id)
    }

    pub fn successful_fraud_proofs(domain_id: DomainId) -> Vec<T::DomainHash> {
        SuccessfulFraudProofs::<T>::get(domain_id)
    }

    pub fn domain_runtime_code(domain_id: DomainId) -> Option<Vec<u8>> {
        RuntimeRegistry::<T>::get(Self::runtime_id(domain_id)?)
            .and_then(|mut runtime_object| runtime_object.raw_genesis.take_runtime_code())
    }

    pub fn domain_best_number(domain_id: DomainId) -> Option<DomainBlockNumberFor<T>> {
        Some(HeadDomainNumber::<T>::get(domain_id))
    }

    pub fn runtime_id(domain_id: DomainId) -> Option<RuntimeId> {
        DomainRegistry::<T>::get(domain_id)
            .map(|domain_object| domain_object.domain_config.runtime_id)
    }

    pub fn domain_instance_data(
        domain_id: DomainId,
    ) -> Option<(DomainInstanceData, BlockNumberFor<T>)> {
        let domain_obj = DomainRegistry::<T>::get(domain_id)?;
        let runtime_object = RuntimeRegistry::<T>::get(domain_obj.domain_config.runtime_id)?;
        let runtime_type = runtime_object.runtime_type.clone();
        let total_issuance = domain_obj.domain_config.total_issuance()?;
        let raw_genesis = runtime_object
            .into_complete_raw_genesis::<T>(
                domain_id,
                domain_obj.domain_runtime_info,
                total_issuance,
                domain_obj.domain_config.initial_balances,
            )
            .ok()?;
        Some((
            DomainInstanceData {
                runtime_type,
                raw_genesis,
            },
            domain_obj.created_at,
        ))
    }

    pub fn genesis_state_root(domain_id: DomainId) -> Option<H256> {
        BlockTree::<T>::get(domain_id, DomainBlockNumberFor::<T>::zero())
            .and_then(BlockTreeNodes::<T>::get)
            .map(|block| block.execution_receipt.final_state_root.into())
    }

    /// Returns the tx range for the domain.
    pub fn domain_tx_range(domain_id: DomainId) -> U256 {
        DomainTxRangeState::<T>::try_get(domain_id)
            .map(|state| state.tx_range)
            .ok()
            .unwrap_or_else(Self::initial_tx_range)
    }

    pub fn bundle_producer_election_params(
        domain_id: DomainId,
    ) -> Option<BundleProducerElectionParams<BalanceOf<T>>> {
        match (
            DomainRegistry::<T>::get(domain_id),
            DomainStakingSummary::<T>::get(domain_id),
        ) {
            (Some(domain_object), Some(stake_summary)) => Some(BundleProducerElectionParams {
                total_domain_stake: stake_summary.current_total_stake,
                bundle_slot_probability: domain_object.domain_config.bundle_slot_probability,
            }),
            _ => None,
        }
    }

    pub fn operator(operator_id: OperatorId) -> Option<(OperatorPublicKey, BalanceOf<T>)> {
        Operators::<T>::get(operator_id)
            .map(|operator| (operator.signing_key, operator.current_total_stake))
    }

    fn check_bundle_duplication(opaque_bundle: &OpaqueBundleOf<T>) -> Result<(), BundleError> {
        // NOTE: it is important to use the hash that not incliude the signature, otherwise
        // the malicious operator may update its `signing_key` (this may support in the future)
        // and sign an existing bundle thus creating a duplicated bundle and pass the check.
        let bundle_header_hash = opaque_bundle.sealed_header.pre_hash();
        ensure!(
            !InboxedBundleAuthor::<T>::contains_key(bundle_header_hash),
            BundleError::DuplicatedBundle
        );
        Ok(())
    }

    fn check_extrinsics_root(opaque_bundle: &OpaqueBundleOf<T>) -> Result<(), BundleError> {
        let expected_extrinsics_root = <T::DomainHeader as Header>::Hashing::ordered_trie_root(
            opaque_bundle
                .extrinsics
                .iter()
                .map(|xt| xt.encode())
                .collect(),
            sp_core::storage::StateVersion::V1,
        );
        ensure!(
            expected_extrinsics_root == opaque_bundle.extrinsics_root(),
            BundleError::InvalidExtrinsicRoot
        );
        Ok(())
    }

    fn check_slot_and_proof_of_time(
        slot_number: u64,
        proof_of_time: PotOutput,
        pre_dispatch: bool,
    ) -> Result<(), BundleError> {
        // NOTE: the `current_block_number` from `frame_system` is initialized during `validate_unsigned` thus
        // it is the same value in both `validate_unsigned` and `pre_dispatch`
        let current_block_number = frame_system::Pallet::<T>::current_block_number();

        // Check if the slot is in future
        //
        // NOTE: during `validate_unsigned` this is implicitly checked within `is_proof_of_time_valid` since we
        // are using quick verification which will return `false` if the `proof-of-time` is not seem by the node
        // before.
        if pre_dispatch {
            if let Some(future_slot) = T::BlockSlot::future_slot(current_block_number) {
                ensure!(slot_number <= *future_slot, BundleError::SlotInTheFuture)
            }
        }

        // Check if the bundle is built too long time ago and beyond `T::BundleLongevity` number of consensus blocks.
        let produced_after_block_number =
            match T::BlockSlot::slot_produced_after(slot_number.into()) {
                Some(n) => n,
                None => {
                    // There is no slot for the genesis block, if the current block is less than `BundleLongevity`
                    // than we assume the slot is produced after the genesis block.
                    if current_block_number > T::BundleLongevity::get().into() {
                        return Err(BundleError::SlotInThePast);
                    } else {
                        Zero::zero()
                    }
                }
            };
        let produced_after_block_hash = if produced_after_block_number == current_block_number {
            // The hash of the current block is only available in the next block thus use the parent hash here
            frame_system::Pallet::<T>::parent_hash()
        } else {
            frame_system::Pallet::<T>::block_hash(produced_after_block_number)
        };
        if let Some(last_eligible_block) =
            current_block_number.checked_sub(&T::BundleLongevity::get().into())
        {
            ensure!(
                produced_after_block_number >= last_eligible_block,
                BundleError::SlotInThePast
            );
        }

        if !is_proof_of_time_valid(
            BlockHash::try_from(produced_after_block_hash.as_ref())
                .expect("Must be able to convert to block hash type"),
            SlotNumber::from(slot_number),
            WrappedPotOutput::from(proof_of_time),
            // Quick verification when entering transaction pool, but not when constructing the block
            !pre_dispatch,
        ) {
            return Err(BundleError::InvalidProofOfTime);
        }

        Ok(())
    }

    fn validate_bundle(
        opaque_bundle: &OpaqueBundleOf<T>,
        pre_dispatch: bool,
    ) -> Result<(), BundleError> {
        let domain_id = opaque_bundle.domain_id();
        let operator_id = opaque_bundle.operator_id();
        let sealed_header = &opaque_bundle.sealed_header;

        let operator = Operators::<T>::get(operator_id).ok_or(BundleError::InvalidOperatorId)?;

        ensure!(
            *operator.status::<T>(operator_id) != OperatorStatus::Slashed
                && *operator.status::<T>(operator_id) != OperatorStatus::PendingSlash,
            BundleError::BadOperator
        );

        if !operator
            .signing_key
            .verify(&sealed_header.pre_hash(), &sealed_header.signature)
        {
            return Err(BundleError::BadBundleSignature);
        }

        Self::check_bundle_duplication(opaque_bundle)?;

        let domain_config = DomainRegistry::<T>::get(domain_id)
            .ok_or(BundleError::InvalidDomainId)?
            .domain_config;

        let domain_bundle_limit = domain_config
            .calculate_bundle_limit::<T>()
            .map_err(|_| BundleError::UnableToCalculateBundleLimit)?;

        ensure!(
            opaque_bundle.body_size() <= domain_bundle_limit.max_bundle_size,
            BundleError::BundleTooLarge
        );

        ensure!(
            opaque_bundle
                .estimated_weight()
                .all_lte(domain_bundle_limit.max_bundle_weight),
            BundleError::BundleTooHeavy
        );

        Self::check_extrinsics_root(opaque_bundle)?;

        let proof_of_election = &sealed_header.header.proof_of_election;
        let (operator_stake, total_domain_stake) =
            Self::fetch_operator_stake_info(domain_id, &operator_id)?;

        Self::check_slot_and_proof_of_time(
            proof_of_election.slot_number,
            proof_of_election.proof_of_time,
            pre_dispatch,
        )?;

        sp_domains::bundle_producer_election::check_proof_of_election(
            &operator.signing_key,
            domain_config.bundle_slot_probability,
            proof_of_election,
            operator_stake.saturated_into(),
            total_domain_stake.saturated_into(),
        )?;

        let receipt = &sealed_header.header.receipt;
        verify_execution_receipt::<T>(domain_id, receipt).map_err(BundleError::Receipt)?;

        Ok(())
    }

    fn validate_fraud_proof(
        fraud_proof: &FraudProof<BlockNumberFor<T>, T::Hash, T::DomainHeader>,
    ) -> Result<(FraudProofTag, TransactionPriority), FraudProofError> {
        let tag_and_priority = if let Some(bad_receipt_hash) =
            fraud_proof.targeted_bad_receipt_hash()
        {
            let bad_receipt = BlockTreeNodes::<T>::get(bad_receipt_hash)
                .ok_or(FraudProofError::BadReceiptNotFound)?
                .execution_receipt;
            let domain_block_number = bad_receipt.domain_block_number;

            ensure!(
                !bad_receipt.domain_block_number.is_zero(),
                FraudProofError::ChallengingGenesisReceipt
            );

            ensure!(
                !Self::is_bad_er_pending_to_prune(
                    fraud_proof.domain_id(),
                    bad_receipt.domain_block_number
                ),
                FraudProofError::BadReceiptAlreadyReported,
            );

            match fraud_proof {
                FraudProof::InvalidBlockFees(InvalidBlockFeesProof { storage_proof, .. }) => {
                    verify_invalid_block_fees_fraud_proof::<
                        T::Block,
                        DomainBlockNumberFor<T>,
                        T::DomainHash,
                        BalanceOf<T>,
                        DomainHashingFor<T>,
                    >(bad_receipt, storage_proof)
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Block fees proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidBlockFeesFraudProof
                    })?;
                }
                FraudProof::InvalidTransfers(req) => {
                    verify_invalid_transfers_fraud_proof::<
                        T::Block,
                        DomainBlockNumberFor<T>,
                        T::DomainHash,
                        BalanceOf<T>,
                        DomainHashingFor<T>,
                    >(bad_receipt, req)
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Domain transfers proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidTransfersFraudProof
                    })?;
                }
                FraudProof::InvalidDomainBlockHash(InvalidDomainBlockHashProof {
                    digest_storage_proof,
                    ..
                }) => {
                    let parent_receipt =
                        BlockTreeNodes::<T>::get(bad_receipt.parent_domain_block_receipt_hash)
                            .ok_or(FraudProofError::ParentReceiptNotFound)?
                            .execution_receipt;
                    verify_invalid_domain_block_hash_fraud_proof::<
                        T::Block,
                        BalanceOf<T>,
                        T::DomainHeader,
                    >(
                        bad_receipt,
                        digest_storage_proof.clone(),
                        parent_receipt.domain_block_hash,
                    )
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Invalid Domain block hash proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidDomainBlockHashFraudProof
                    })?;
                }
                FraudProof::InvalidExtrinsicsRoot(proof) => {
                    verify_invalid_domain_extrinsics_root_fraud_proof::<
                        T::Block,
                        BalanceOf<T>,
                        T::Hashing,
                        T::DomainHeader,
                    >(bad_receipt, proof)
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Invalid Domain extrinsic root proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidExtrinsicRootFraudProof
                    })?;
                }
                FraudProof::InvalidStateTransition(proof) => {
                    let bad_receipt_parent =
                        BlockTreeNodes::<T>::get(bad_receipt.parent_domain_block_receipt_hash)
                            .ok_or(FraudProofError::ParentReceiptNotFound)?
                            .execution_receipt;

                    verify_invalid_state_transition_fraud_proof::<
                        T::Block,
                        T::DomainHeader,
                        BalanceOf<T>,
                    >(bad_receipt, bad_receipt_parent, proof)
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Invalid State transition proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidStateTransitionFraudProof
                    })?;
                }
                FraudProof::InvalidBundles(invalid_bundles_fraud_proof) => {
                    let bad_receipt_parent =
                        BlockTreeNodes::<T>::get(bad_receipt.parent_domain_block_receipt_hash)
                            .ok_or(FraudProofError::ParentReceiptNotFound)?
                            .execution_receipt;

                    verify_invalid_bundles_fraud_proof::<T::Block, T::DomainHeader, BalanceOf<T>>(
                        bad_receipt,
                        bad_receipt_parent,
                        invalid_bundles_fraud_proof,
                    )
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Invalid Bundle proof verification failed: {err:?}"
                        );
                        FraudProofError::InvalidBundleFraudProof
                    })?;
                }
                FraudProof::ValidBundle(proof) => verify_valid_bundle_fraud_proof::<
                    T::Block,
                    DomainBlockNumberFor<T>,
                    T::DomainHash,
                    BalanceOf<T>,
                >(bad_receipt, proof)
                .map_err(|err| {
                    log::error!(
                        target: "runtime::domains",
                        "Valid bundle proof verification failed: {err:?}"
                    );
                    FraudProofError::BadValidBundleFraudProof
                })?,
                _ => return Err(FraudProofError::UnexpectedFraudProof),
            }

            // The priority of fraud proof is determined by how many blocks left before the bad ER
            // is confirmed, the less the more emergency it is, thus give a higher priority.
            let block_before_bad_er_confirm = domain_block_number.saturating_sub(
                Self::latest_confirmed_domain_block_number(fraud_proof.domain_id()),
            );
            let priority =
                TransactionPriority::MAX - block_before_bad_er_confirm.saturated_into::<u64>();

            // Use the domain id as tag thus the consensus node only accept one fraud proof for a
            // specific domain at a time
            let tag = FraudProofTag::BadER(fraud_proof.domain_id());

            (tag, priority)
        } else if let Some((bad_operator_id, _)) =
            fraud_proof.targeted_bad_operator_and_slot_for_bundle_equivocation()
        {
            let operator =
                Operators::<T>::get(bad_operator_id).ok_or(FraudProofError::MissingOperator)?;
            match fraud_proof {
                FraudProof::BundleEquivocation(proof) => {
                    let operator_signing_key = operator.signing_key;
                    verify_bundle_equivocation_fraud_proof::<T::Block, T::DomainHeader, Balance>(
                        &operator_signing_key,
                        &proof.first_header,
                        &proof.second_header,
                    )
                    .map_err(|err| {
                        log::error!(
                            target: "runtime::domains",
                            "Bundle equivocation proof verification failed: {err:?}"
                        );
                        FraudProofError::BadBundleEquivocationFraudProof
                    })?;
                }

                _ => return Err(FraudProofError::UnexpectedFraudProof),
            }

            // Bundle equivacotion fraud proof doesn't target bad ER thus we give it the lowest priority
            // compared to other fraud proofs
            let priority = TransactionPriority::MAX
                - T::BlockTreePruningDepth::get().saturated_into::<u64>()
                - 1;

            // Use the operator id as tag thus the consensus node only accept one bundle equivacotion fraud proof
            // for a specific operator at a time
            let tag = FraudProofTag::BundleEquivocation(bad_operator_id);

            (tag, priority)
        } else {
            return Err(FraudProofError::UnexpectedFraudProof);
        };

        Ok(tag_and_priority)
    }

    /// Return operators specific election verification params for Proof of Election verification.
    /// If there was an epoch transition in this block for this domain,
    ///     then return the parameters from previous epoch stored in LastEpochStakingDistribution
    /// Else, return those details from the Domain's stake summary for this epoch.
    fn fetch_operator_stake_info(
        domain_id: DomainId,
        operator_id: &OperatorId,
    ) -> Result<(BalanceOf<T>, BalanceOf<T>), BundleError> {
        if let Some(pending_election_params) = LastEpochStakingDistribution::<T>::get(domain_id) {
            if let Some(operator_stake) = pending_election_params.operators.get(operator_id) {
                return Ok((*operator_stake, pending_election_params.total_domain_stake));
            }
        }
        let domain_stake_summary =
            DomainStakingSummary::<T>::get(domain_id).ok_or(BundleError::InvalidDomainId)?;
        let operator_stake = domain_stake_summary
            .current_operators
            .get(operator_id)
            .ok_or(BundleError::BadOperator)?;
        Ok((*operator_stake, domain_stake_summary.current_total_stake))
    }

    /// Called when a bundle is added to update the bundle state for tx range
    /// calculation.
    #[allow(dead_code)]
    // TODO: use once we support tx-range dynamic adjustment properly
    fn note_domain_bundle(domain_id: DomainId) {
        DomainTxRangeState::<T>::mutate(domain_id, |maybe_state| match maybe_state {
            Some(state) => {
                state.interval_bundles += 1;
            }
            None => {
                maybe_state.replace(TxRangeState {
                    tx_range: Self::initial_tx_range(),
                    interval_blocks: 0,
                    interval_bundles: 1,
                });
            }
        });
    }

    /// Called when the block is finalized to update the tx range for all the
    /// domains with bundles in the block.
    #[allow(dead_code)]
    // TODO: use once we support tx-range dynamic adjustment properly
    fn update_domain_tx_range() {
        for domain_id in DomainTxRangeState::<T>::iter_keys() {
            if let Some(domain_config) =
                DomainRegistry::<T>::get(domain_id).map(|obj| obj.domain_config)
            {
                DomainTxRangeState::<T>::mutate(domain_id, |maybe_tx_range_state| {
                    if let Some(tx_range_state) = maybe_tx_range_state {
                        let tx_range_adjustment_interval =
                            T::DomainTxRangeAdjustmentInterval::get();

                        tx_range_state.interval_blocks += 1;

                        if tx_range_state.interval_blocks < tx_range_adjustment_interval {
                            return;
                        }

                        // End of interval, calculate the new tx range.
                        let TxRangeState {
                            tx_range,
                            interval_blocks,
                            interval_bundles,
                        } = tx_range_state;

                        let actual_bundle_count = *interval_bundles;
                        let expected_bundle_count = tx_range_adjustment_interval
                            * u64::from(domain_config.target_bundles_per_block);

                        let new_tx_range = calculate_tx_range(
                            *tx_range,
                            actual_bundle_count,
                            expected_bundle_count,
                        );

                        log::trace!(
                            target: "runtime::domains",
                            "tx range update: blocks = {interval_blocks}, bundles = {actual_bundle_count}, prev = {tx_range}, new = {new_tx_range}"
                        );

                        // Reset the tx range and start over.
                        tx_range_state.tx_range = new_tx_range;
                        tx_range_state.interval_blocks = 0;
                        tx_range_state.interval_bundles = 0;
                    }
                })
            }
        }
    }

    /// Calculates the initial tx range.
    fn initial_tx_range() -> U256 {
        U256::MAX / T::InitialDomainTxRange::get()
    }

    /// Returns the best execution chain number.
    pub fn head_receipt_number(domain_id: DomainId) -> DomainBlockNumberFor<T> {
        HeadReceiptNumber::<T>::get(domain_id)
    }

    /// Returns the block number of the oldest existing unconfirmed execution receipt, return `None`
    /// means there is no unconfirmed ER exist or submitted yet.
    pub fn oldest_unconfirmed_receipt_number(
        domain_id: DomainId,
    ) -> Option<DomainBlockNumberFor<T>> {
        let oldest_nonconfirmed_er_number =
            Self::latest_confirmed_domain_block_number(domain_id).saturating_add(One::one());
        let is_er_exist = BlockTree::<T>::get(domain_id, oldest_nonconfirmed_er_number).is_some();
        let is_pending_to_prune =
            Self::is_bad_er_pending_to_prune(domain_id, oldest_nonconfirmed_er_number);

        if is_er_exist && !is_pending_to_prune {
            Some(oldest_nonconfirmed_er_number)
        } else {
            // The `oldest_nonconfirmed_er_number` ER may not exist if
            // - The domain just started and no ER submitted yet
            // - The oldest ER just pruned by fraud proof and no new ER submitted yet
            // - When using consensus block to derive the challenge period forward (unimplemented yet)
            None
        }
    }

    /// Returns the latest confirmed domain block number for a given domain
    /// Zero block is always a default confirmed block.
    pub fn latest_confirmed_domain_block_number(domain_id: DomainId) -> DomainBlockNumberFor<T> {
        LatestConfirmedDomainBlock::<T>::get(domain_id)
            .map(|block| block.block_number)
            .unwrap_or_default()
    }

    pub fn latest_confirmed_domain_block(
        domain_id: DomainId,
    ) -> Option<(DomainBlockNumberFor<T>, T::DomainHash)> {
        LatestConfirmedDomainBlock::<T>::get(domain_id)
            .map(|block| (block.block_number, block.block_hash))
    }

    /// Returns the domain block limit of the given domain.
    pub fn domain_block_limit(domain_id: DomainId) -> Option<DomainBlockLimit> {
        DomainRegistry::<T>::get(domain_id).map(|domain_obj| DomainBlockLimit {
            max_block_size: domain_obj.domain_config.max_block_size,
            max_block_weight: domain_obj.domain_config.max_block_weight,
        })
    }

    /// Returns the domain bundle limit of the given domain
    pub fn domain_bundle_limit(
        domain_id: DomainId,
    ) -> Result<Option<DomainBundleLimit>, DomainRegistryError> {
        let domain_config = match DomainRegistry::<T>::get(domain_id) {
            None => return Ok(None),
            Some(domain_obj) => domain_obj.domain_config,
        };

        let bundle_limit = domain_config.calculate_bundle_limit::<T>()?;

        Ok(Some(bundle_limit))
    }

    /// Returns if there are any ERs in the challenge period that have non empty extrinsics.
    /// Note that Genesis ER is also considered special and hence non empty
    pub fn non_empty_er_exists(domain_id: DomainId) -> bool {
        if BlockTree::<T>::contains_key(domain_id, DomainBlockNumberFor::<T>::zero()) {
            return true;
        }

        // Start from the oldest non-confirmed ER to the head domain number
        let mut to_check =
            Self::latest_confirmed_domain_block_number(domain_id).saturating_add(One::one());
        let head_number = HeadDomainNumber::<T>::get(domain_id);

        while to_check <= head_number {
            if !ExecutionInbox::<T>::iter_prefix_values((domain_id, to_check)).all(|digests| {
                digests
                    .iter()
                    .all(|digest| digest.extrinsics_root == EMPTY_EXTRINSIC_ROOT.into())
            }) {
                return true;
            }

            to_check = to_check.saturating_add(One::one())
        }

        false
    }

    pub fn extrinsics_shuffling_seed() -> T::Hash {
        let seed = DOMAIN_EXTRINSICS_SHUFFLING_SEED_SUBJECT;
        let (randomness, _) = T::Randomness::random(seed);
        randomness
    }

    pub fn execution_receipt(receipt_hash: ReceiptHashFor<T>) -> Option<ExecutionReceiptOf<T>> {
        BlockTreeNodes::<T>::get(receipt_hash).map(|db| db.execution_receipt)
    }

    pub fn receipt_hash(
        domain_id: DomainId,
        domain_number: DomainBlockNumberFor<T>,
    ) -> Option<ReceiptHashFor<T>> {
        BlockTree::<T>::get(domain_id, domain_number)
    }

    pub fn confirmed_domain_block_storage_key(domain_id: DomainId) -> Vec<u8> {
        LatestConfirmedDomainBlock::<T>::hashed_key_for(domain_id)
    }

    pub fn is_bad_er_pending_to_prune(
        domain_id: DomainId,
        receipt_number: DomainBlockNumberFor<T>,
    ) -> bool {
        // The genesis receipt is always valid
        if receipt_number.is_zero() {
            return false;
        }

        let head_receipt_number = HeadReceiptNumber::<T>::get(domain_id);

        // If `receipt_number` is greater than the current `head_receipt_number` meaning it is a
        // bad ER and the `head_receipt_number` is previously reverted by a fraud proof
        head_receipt_number < receipt_number
    }

    pub fn is_operator_pending_to_slash(domain_id: DomainId, operator_id: OperatorId) -> bool {
        let latest_submitted_er = LatestSubmittedER::<T>::get((domain_id, operator_id));

        // The genesis receipt is always valid
        if latest_submitted_er.is_zero() {
            return false;
        }

        let head_receipt_number = HeadReceiptNumber::<T>::get(domain_id);

        // If the operator have submitted an ER greater than the current `head_receipt_number`
        // meaning the ER is a bad ER and the `head_receipt_number` is previously reverted by
        // a fraud proof
        head_receipt_number < latest_submitted_er
    }

    pub fn max_submit_bundle_weight() -> Weight {
        T::WeightInfo::submit_bundle()
            .saturating_add(
                // NOTE: within `submit_bundle`, only one of (or none) `handle_bad_receipt` and
                // `confirm_domain_block` can happen, thus we use the `max` of them
                T::WeightInfo::handle_bad_receipt(T::MaxNominators::get()).max(
                    T::WeightInfo::confirm_domain_block(MAX_BUNLDE_PER_BLOCK, MAX_BUNLDE_PER_BLOCK),
                ),
            )
            .saturating_add(Self::max_staking_epoch_transition())
    }

    pub fn max_staking_epoch_transition() -> Weight {
        T::WeightInfo::operator_reward_tax_and_restake(MAX_BUNLDE_PER_BLOCK)
            .saturating_add(T::WeightInfo::finalize_slashed_operators(
                // FIXME: the actual value should be `N * T::MaxNominators` where `N` is the number of
                // submitter of the bad ER, which is probabilistically bounded by `bundle_slot_probability`
                // we use `N = 1` here because `finalize_slashed_operators` is expensive and can consume
                // more weight than the max block weight
                T::MaxNominators::get(),
            ))
            .saturating_add(T::WeightInfo::finalize_domain_epoch_staking(
                T::MaxPendingStakingOperation::get(),
            ))
    }

    fn actual_epoch_transition_weight(epoch_transition_res: EpochTransitionResult) -> Weight {
        let EpochTransitionResult {
            rewarded_operator_count,
            slashed_nominator_count,
            finalized_operator_count,
            ..
        } = epoch_transition_res;

        T::WeightInfo::operator_reward_tax_and_restake(rewarded_operator_count)
            .saturating_add(T::WeightInfo::finalize_slashed_operators(
                slashed_nominator_count,
            ))
            .saturating_add(T::WeightInfo::finalize_domain_epoch_staking(
                finalized_operator_count,
            ))
    }

    pub fn storage_fund_account_balance(operator_id: OperatorId) -> BalanceOf<T> {
        let storage_fund_acc = storage_fund_account::<T>(operator_id);
        T::Currency::reducible_balance(&storage_fund_acc, Preservation::Preserve, Fortitude::Polite)
    }
}

impl<T: Config> sp_domains::DomainOwner<T::AccountId> for Pallet<T> {
    fn is_domain_owner(domain_id: DomainId, acc: T::AccountId) -> bool {
        if let Some(domain_obj) = DomainRegistry::<T>::get(domain_id) {
            domain_obj.owner_account_id == acc
        } else {
            false
        }
    }
}

impl<T> Pallet<T>
where
    T: Config + frame_system::offchain::SendTransactionTypes<Call<T>>,
{
    /// Submits an unsigned extrinsic [`Call::submit_bundle`].
    pub fn submit_bundle_unsigned(opaque_bundle: OpaqueBundleOf<T>) {
        let slot = opaque_bundle.sealed_header.slot_number();
        let extrincis_count = opaque_bundle.extrinsics.len();

        let call = Call::submit_bundle { opaque_bundle };

        match SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into()) {
            Ok(()) => {
                log::info!(
                    target: "runtime::domains",
                    "Submitted bundle from slot {slot}, extrinsics: {extrincis_count}",
                );
            }
            Err(()) => {
                log::error!(target: "runtime::domains", "Error submitting bundle");
            }
        }
    }

    /// Submits an unsigned extrinsic [`Call::submit_fraud_proof`].
    pub fn submit_fraud_proof_unsigned(
        fraud_proof: FraudProof<BlockNumberFor<T>, T::Hash, T::DomainHeader>,
    ) {
        let call = Call::submit_fraud_proof {
            fraud_proof: Box::new(fraud_proof),
        };

        match SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into()) {
            Ok(()) => {
                log::info!(target: "runtime::domains", "Submitted fraud proof");
            }
            Err(()) => {
                log::error!(target: "runtime::domains", "Error submitting fraud proof");
            }
        }
    }
}

/// Calculates the new tx range based on the bundles produced during the interval.
pub fn calculate_tx_range(
    cur_tx_range: U256,
    actual_bundle_count: u64,
    expected_bundle_count: u64,
) -> U256 {
    if actual_bundle_count == 0 || expected_bundle_count == 0 {
        return cur_tx_range;
    }

    let Some(new_tx_range) = U256::from(actual_bundle_count)
        .saturating_mul(&cur_tx_range)
        .checked_div(&U256::from(expected_bundle_count))
    else {
        return cur_tx_range;
    };

    let upper_bound = cur_tx_range.saturating_mul(&U256::from(4_u64));
    let Some(lower_bound) = cur_tx_range.checked_div(&U256::from(4_u64)) else {
        return cur_tx_range;
    };
    new_tx_range.clamp(lower_bound, upper_bound)
}
