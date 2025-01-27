// SPDX-License-Identifier: GPL-3.0-or-later


#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
mod default_weights;
mod migrations;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use codec::{Decode, Encode};
use frame_support::{
	decl_error, decl_event, decl_module, decl_storage, ensure,
	traits::{
		Currency, Get, LockIdentifier, LockableCurrency,
		WithdrawReasons,
	},
	weights::Weight,
};
use frame_system::{ensure_root, ensure_signed};
use frame_support::pallet_prelude::EnsureOrigin;
use sp_arithmetic::per_things::Rounding;
use sp_consensus_poscan::POSCAN_ENGINE_ID;
use sp_runtime::traits::{Saturating, Zero};
use sp_runtime::{Perbill, Percent};
use sp_std::{
	collections::btree_map::BTreeMap, iter::FromIterator, ops::Bound::Included, prelude::*,
};
use sp_std::convert::TryInto;
use scale_info::TypeInfo;

use log;
use rewards_api::RewardLocksApi;
use validator_set_api::ValidatorSetApi;
use mining_pool_stat_api::MiningPoolStatApi;
use sp_consensus_poscan::Difficulty;
pub const LOG_TARGET: &'static str = "runtime::validator-set";


pub struct LockBounds {
	pub period_max: u16,
	pub period_min: u16,
	pub divide_max: u16,
	pub divide_min: u16,
}

#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, Debug, TypeInfo)]
pub struct LockParameters {
	pub period: u16,
	pub divide: u16,
}

/// Trait for generating reward locks.
pub trait GenerateRewardLocks<T: Config> {
	/// Generate reward locks.
	fn generate_reward_locks(
		current_block: T::BlockNumber,
		total_reward: BalanceOf<T>,
		lock_parameters: Option<LockParameters>,
	) -> BTreeMap<T::BlockNumber, BalanceOf<T>>;

	fn max_locks(lock_bounds: LockBounds) -> u32;

	fn calc_rewards(when: T::BlockNumber) -> BalanceOf<T>;
}

pub trait WeightInfo {
	fn on_initialize() -> Weight;
	fn on_finalize() -> Weight;
	fn unlock() -> Weight;
	fn set_schedule() -> Weight;
	fn set_lock_params() -> Weight;
	fn set_miner_share() -> Weight;
}

/// Config for rewards.
pub trait Config: frame_system::Config + pallet_treasury::Config + pallet_balances::Config {
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;
	/// An implementation of on-chain currency.
	type Currency: LockableCurrency<Self::AccountId>;
	/// Donation destination.
	type DonationDestination: Get<Self::AccountId>;
	/// Generate reward locks.
	type GenerateRewardLocks: GenerateRewardLocks<Self>;
	/// Weights for this pallet.
	type WeightInfo: WeightInfo;
	/// Lock Parameters Bounds.
	type LockParametersBounds: Get<LockBounds>;
	/// Pallet validator
	type ValidatorSet: ValidatorSetApi<Self::AccountId, Self::BlockNumber, BalanceOf::<Self>>;
	/// Percent of rewards for miner
	type MinerRewardsPercent: Get<Percent>;
	/// Percent of rewards for miner
	type MiningPool: MiningPoolStatApi<Difficulty, Self::AccountId>;
	/// Max pool rate
	type MiningPoolMaxRate: Get<Percent>;
	/// Miner share origin
	type MinerShareOrigin: EnsureOrigin<Self::Origin>;
}

/// Type alias for currency balance.
pub type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

decl_error! {
	pub enum Error for Module<T: Config> {
		/// Reward set is too low.
		RewardTooLow,
		/// Mint value is too low.
		MintTooLow,
		/// Reward curve is not sorted.
		NotSorted,
		/// Lock parameters are out of bounds.
		LockParamsOutOfBounds,
		/// Lock period is not a multiple of the divide.
		LockPeriodNotDivisible,
		/// Insufficient balance
		UnsufficientBalance,
		/// decrease lock amount not allowed .
		DecreaseLockAmountNotAllowed,
	}
}

decl_module! {
	pub struct Module<T: Config> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		fn deposit_event() = default;

		fn on_initialize(now: T::BlockNumber) -> Weight {
			let author = frame_system::Pallet::<T>::digest()
				.logs
				.iter()
				.filter_map(|s| s.as_pre_runtime())
				.filter_map(|(id, mut data)| if id == POSCAN_ENGINE_ID {
					T::AccountId::decode(&mut data).ok()
				} else {
					None
				})
				.next();

			if let Some(author) = author {
				<Self as Store>::Author::put(author);
			}

			let cur_block_number = <frame_system::Pallet<T>>::block_number();
			let cur_reward = T::GenerateRewardLocks::calc_rewards(cur_block_number);
			let d = u128::from_le_bytes(cur_reward.encode().try_into().unwrap());

			log::debug!(target: LOG_TARGET, "cur_reward: {}", d);

			Reward::<T>::set(cur_reward);

			RewardChanges::<T>::mutate(|reward_changes| {
				let mut removing = Vec::new();

				for (block_number, reward) in reward_changes.range((Included(Zero::zero()), Included(now))) {
					Reward::<T>::set(*reward);
					removing.push(*block_number);

					Self::deposit_event(Event::<T>::RewardChanged(*reward));
				}

				for block_number in removing {
					reward_changes.remove(&block_number);
				}
			});

			MintChanges::<T>::mutate(|mint_changes| {
				let mut removing = Vec::new();

				for (block_number, mints) in mint_changes.range((Included(Zero::zero()), Included(now))) {
					Mints::<T>::set(mints.clone());
					removing.push(*block_number);

					Self::deposit_event(Event::<T>::MintsChanged(mints.clone()));
				}

				for block_number in removing {
					mint_changes.remove(&block_number);
				}
			});

			<T as Config>::WeightInfo::on_initialize().saturating_add(<T as Config>::WeightInfo::on_finalize())
		}

		fn on_finalize(now: T::BlockNumber) {
			if let Some(author) = <Self as Store>::Author::get() {
				let reward = Reward::<T>::get();
				Self::do_reward(&author, reward, now);
			}

			let mints = Mints::<T>::get();
			Self::do_mints(&mints);

			<Self as Store>::Author::kill();
		}

		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let version = StorageVersion::get();
			let new_version = version.migrate::<T>();
			StorageVersion::put(new_version);

			0
		}

		#[weight = <T as Config>::WeightInfo::set_schedule()]
		fn set_schedule(
			origin,
			reward: BalanceOf<T>,
			mints: Vec<(T::AccountId, BalanceOf<T>)>,
			reward_changes: Vec<(T::BlockNumber, BalanceOf<T>)>,
			mint_changes: Vec<(T::BlockNumber, Vec<(T::AccountId, BalanceOf<T>)>)>,
		) {
			ensure_root(origin)?;

			let mints = BTreeMap::from_iter(mints.into_iter());
			let reward_changes = BTreeMap::from_iter(reward_changes.into_iter());
			let mint_changes = BTreeMap::from_iter(
				mint_changes.into_iter().map(|(k, v)| (k, BTreeMap::from_iter(v.into_iter())))
			);

			ensure!(reward >= <T as Config>::Currency::minimum_balance(), Error::<T>::RewardTooLow);
			for (_, mint) in &mints {
				ensure!(*mint >= <T as Config>::Currency::minimum_balance(), Error::<T>::MintTooLow);
			}
			for (_, reward_change) in &reward_changes {
				ensure!(*reward_change >= <T as Config>::Currency::minimum_balance(), Error::<T>::RewardTooLow);
			}
			for (_, mint_change) in &mint_changes {
				for (_, mint) in mint_change {
					ensure!(*mint >= <T as Config>::Currency::minimum_balance(), Error::<T>::MintTooLow);
				}
			}

			Reward::<T>::put(reward);
			Self::deposit_event(RawEvent::RewardChanged(reward));

			Mints::<T>::put(mints.clone());
			Self::deposit_event(RawEvent::MintsChanged(mints));

			RewardChanges::<T>::put(reward_changes);
			MintChanges::<T>::put(mint_changes);
			Self::deposit_event(RawEvent::ScheduleSet);
		}

		#[weight = <T as Config>::WeightInfo::set_lock_params()]
		fn set_lock_params(origin, lock_params: LockParameters) {
			ensure_root(origin)?;

			let bounds = T::LockParametersBounds::get();
			ensure!((bounds.period_min..=bounds.period_max).contains(&lock_params.period) &&
				(bounds.divide_min..=bounds.divide_max).contains(&lock_params.divide), Error::<T>::LockParamsOutOfBounds);
			ensure!(lock_params.period % lock_params.divide == 0, Error::<T>::LockPeriodNotDivisible);

			LockParams::put(lock_params);
			Self::deposit_event(RawEvent::LockParamsChanged(lock_params));
		}

		/// Set miner rewards.
		#[weight = <T as Config>::WeightInfo::set_miner_share()]
		fn set_miner_share(origin, pct: Percent) {
			T::MinerShareOrigin::ensure_origin(origin)?;

			if pct >= Percent::zero() && pct <= Percent::one() {
				<Self as Store>::MinerShare::put(pct);
				Self::deposit_event(RawEvent::MinerShare(pct));
			}
		}

		/// Unlock any vested rewards for `target` account.
		#[weight = <T as Config>::WeightInfo::unlock()]
		fn unlock(origin) {
			let target = ensure_signed(origin)?;

			let locks = Self::reward_locks(&target);
			let current_number = frame_system::Pallet::<T>::block_number();
			Self::do_update_reward_locks(&target, locks, current_number, false);
		}

		#[weight = 0]
		fn force_unlock(
			origin,
			account_id: T::AccountId,
		) {
			ensure_root(origin)?;

			let locks = Self::reward_locks(&account_id);
			let current_number = frame_system::Pallet::<T>::block_number();
			Self::do_update_reward_locks(&account_id, locks, current_number, true);
		}
	}
}

decl_storage! {
	trait Store for Module<T: Config> as Rewards {
		/// Current block author.
		Author get(fn author): Option<T::AccountId>;

		/// Current block reward for miner.
		Reward get(fn reward) config(): BalanceOf<T>;
		/// Pending reward locks.
		RewardLocks get(fn reward_locks): map hasher(twox_64_concat) T::AccountId => BTreeMap<T::BlockNumber, BalanceOf<T>>;
		/// Reward changes planned in the future.
		RewardChanges get(fn reward_changes): BTreeMap<T::BlockNumber, BalanceOf<T>>;

		/// Current block mints.
		Mints get(fn mints) config(): BTreeMap<T::AccountId, BalanceOf<T>>;
		/// Mint changes planned in the future.
		MintChanges get(fn mint_changes): BTreeMap<T::BlockNumber, BTreeMap<T::AccountId, BalanceOf<T>>>;

		/// Lock parameters (period and divide).
		LockParams get(fn lock_params): Option<LockParameters>;

		/// Miner share percent.
		MinerShare get(fn miner_percent): Option<Percent>;

		StorageVersion build(|_| migrations::StorageVersion::V1): migrations::StorageVersion;
	}
}

decl_event! {
	pub enum Event<T> where AccountId = <T as frame_system::Config>::AccountId, Balance = BalanceOf<T> {
		/// A new schedule has been set.
		ScheduleSet,
		/// Reward has been sent.
		Rewarded(AccountId, Balance),
		/// Reward has been changed.
		RewardChanged(Balance),
		/// Mint has been sent.
		Minted(AccountId, Balance),
		/// Mint has been changed.
		MintsChanged(BTreeMap<AccountId, Balance>),
		/// Lock Parameters have been changed.
		LockParamsChanged(LockParameters),
		/// Lock set.
		Locked(AccountId, Balance),
		/// Miner slashed.
		MinerSlash(AccountId, Balance),
		/// Miner share has changed .
		MinerShare(Percent),
		/// Miner slashed.
		PoolExceedsLimit(AccountId, Balance),
	}
}
// Must be the same as in validator-set pallet
const REWARDS_ID: LockIdentifier = *b"rewards ";

impl<T: Config> Module<T> {
	fn do_reward(author: &T::AccountId, reward: BalanceOf<T>, when: T::BlockNumber) {
		let miner_share = <Self as Store>::MinerShare::get()
			.unwrap_or_else(|| T::MinerRewardsPercent::get());
		let mut miner_total = miner_share * reward;

		log::debug!(target: LOG_TARGET, "miner_total: {:?}", miner_total);

		let pool_stat = T::MiningPool::get_stat(author);
		if let Some(pool_stat) = pool_stat {
			let pool_rate = Perbill::from_percent(pool_stat.1.deconstruct().into());
			let limit = Perbill::from_percent(T::MiningPoolMaxRate::get().deconstruct().into());
			let overmined = if pool_rate <= limit {
				Perbill::zero()
			}
			else if pool_rate >= limit.int_mul(2) {
				Perbill::one()
			}
			else {
				Perbill::saturating_div(pool_rate - limit ,limit , Rounding::NearestPrefDown)
			};

			let slash_amount = overmined * miner_total;
			if overmined > Perbill::zero() {
				let pot_id = pallet_treasury::Pallet::<T>::account_id();
				drop(<T as Config>::Currency::deposit_creating(&pot_id, slash_amount));
				Self::deposit_event(Event::<T>::PoolExceedsLimit(author.clone(), slash_amount));
			}
			miner_total = miner_total.saturating_sub(slash_amount);
			log::trace!(target: LOG_TARGET, "miner_total: {:?}", miner_total);

			let pool_total = pool_stat.0 * miner_total;
			log::trace!(target: LOG_TARGET, "pool_total: {:?}", pool_total);

			let members_total = miner_total.saturating_sub(pool_total);
			let sum_weight = pool_stat.2.iter().map(|a| a.1 as u64).sum();
			let tot_weight = if sum_weight == 0 { pool_stat.2.iter().count() as u64 } else { sum_weight };

			let mut payed_rewards: BalanceOf<T> = Zero::zero();
			for (member_id, w) in pool_stat.2.iter() {
				let w = if sum_weight == 0 { 1 } else { *w };
				let rewards = Perbill::from_rational(w as u64, tot_weight) * members_total;
				log::trace!(target: LOG_TARGET, "miner_member_reword: {:?}", rewards);
				Self::do_reward_per_account(&member_id, rewards, when);
				payed_rewards = payed_rewards.saturating_add(rewards);
			}
			miner_total = pool_total.saturating_add(members_total.saturating_sub(payed_rewards));
		}
		Self::do_reward_per_account(author, miner_total, when);
		let validator_total = reward.saturating_sub(miner_total);

		log::trace!(target: LOG_TARGET, "miner_reword: {:?}", miner_total);

		let validators = T::ValidatorSet::validators();

		let n_val: usize = validators.len();
		let per_val = Perbill::from_rational(1, n_val as u32) * validator_total;

		for val in validators.iter() {
			log::trace!(target: LOG_TARGET, "validator_reword: {:?} for {:?}", per_val, val.encode());
			Self::do_reward_per_account(val, per_val, when);
		}
	}

	fn do_reward_per_account(account: &T::AccountId, reward: BalanceOf<T>, when: T::BlockNumber) {
		let account_reward_locks =
			T::GenerateRewardLocks::generate_reward_locks(when, reward, LockParams::get());

		drop(<T as Config>::Currency::deposit_creating(&account, reward));

		if account_reward_locks.len() > 0 {
			let mut locks = Self::reward_locks(&account);

			for (new_lock_number, new_lock_balance) in account_reward_locks {
				let old_balance = *locks
					.get(&new_lock_number)
					.unwrap_or(&BalanceOf::<T>::default());
				let new_balance = old_balance.saturating_add(new_lock_balance);
				locks.insert(new_lock_number, new_balance);
			}

			Self::do_update_reward_locks(&account, locks, when, false);
		}
	}

	fn do_update_reward_locks(
		author: &T::AccountId,
		mut locks: BTreeMap<T::BlockNumber, BalanceOf<T>>,
		current_number: T::BlockNumber,
		force: bool,
	) {
		let mut expired = Vec::new();

		if force {
			for (block_number, _) in &locks {
				expired.push(*block_number);
			}

			for block_number in expired {
				locks.remove(&block_number);
			}

			<T as Config>::Currency::remove_lock(
				REWARDS_ID,
				&author,
			);
		}
		else {
			let mut total_locked: BalanceOf<T> = Zero::zero();

			for (block_number, locked_balance) in &locks {
				if block_number <= &current_number {
					expired.push(*block_number);
				} else {
					total_locked = total_locked.saturating_add(*locked_balance);
				}
			}

			for block_number in expired {
				locks.remove(&block_number);
			}

			<T as Config>::Currency::set_lock(
				REWARDS_ID,
				&author,
				total_locked,
				WithdrawReasons::except(WithdrawReasons::TRANSACTION_PAYMENT),
			);
		}

		<Self as Store>::RewardLocks::insert(author, locks);
	}

	fn do_mints(mints: &BTreeMap<T::AccountId, BalanceOf<T>>) {
		for (destination, mint) in mints {
			drop(<T as Config>::Currency::deposit_creating(&destination, *mint));
		}
	}
}

impl<T: Config> RewardLocksApi<T::AccountId, BalanceOf<T>> for Pallet<T> {
	fn locks(account_id: &T::AccountId) -> BalanceOf<T> {
		Self::reward_locks(account_id)
			.iter()
			.fold(
				Zero::zero(),
				|s, (_block_number, locked_balance)| s.saturating_add(*locked_balance)
			)
	}
}
