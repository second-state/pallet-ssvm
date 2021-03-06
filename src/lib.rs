// Copyright (C) 2020 Second State.
// This file is part of Pallet-SSVM.

// Pallet-SSVM is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.

// Pallet-SSVM is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.

// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! SSVM execution module for Substrate

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod backend;

#[cfg(feature = "std")]
use crate::backend::HostContext;
pub use crate::backend::{create_address, Account, Log, TxContext};
use frame_support::traits::{Currency, ExistenceRequirement, WithdrawReason};
use frame_support::weights::SimpleDispatchInfo;
use frame_support::weights::{DispatchClass, FunctionOf, Weight};
use frame_support::{decl_error, decl_event, decl_module, decl_storage};
use frame_system::{self as system, ensure_signed};
#[cfg(feature = "std")]
use lazy_static::lazy_static;
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use sp_core::{Hasher, H160, H256, U256};
use sp_runtime::ModuleId;
use sp_runtime::{
    traits::{AccountIdConversion, SaturatedConversion, UniqueSaturatedInto},
    DispatchResult,
};
use sp_std::convert::TryInto;
use sp_std::{if_std, marker::PhantomData, vec::Vec};
#[cfg(feature = "std")]
use ssvm::types::{CallKind, Revision, StatusCode, StorageStatus};
#[cfg(feature = "std")]
use std::collections::HashMap;
const MODULE_ID: ModuleId = ModuleId(*b"ssvmmoid");
#[cfg(feature = "std")]
use std::sync::Mutex;

/// Type alias for currency balance.
pub type BalanceOf<T> =
    <<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;

/// Trait for converting account ids of `balances` module into
/// `H160` for EVM module.
///
/// Accounts and contracts of this module are stored in its own
/// storage, in an Ethereum-compatible format. In order to communicate
/// with the rest of Substrate module, we require an one-to-one
/// mapping of Substrate account to Ethereum address.
pub trait ConvertAccountId<A> {
    /// Given a Substrate address, return the corresponding Ethereum address.
    fn convert_account_id(account_id: &A) -> H160;
}

/// Hash and then truncate the account id, taking the last 160-bit as the Ethereum address.
pub struct HashTruncateConvertAccountId<H>(PhantomData<H>);

impl<H: Hasher> Default for HashTruncateConvertAccountId<H> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<H: Hasher, A: AsRef<[u8]>> ConvertAccountId<A> for HashTruncateConvertAccountId<H> {
    fn convert_account_id(account_id: &A) -> H160 {
        let account_id = H::hash(account_id.as_ref());
        let account_id_len = account_id.as_ref().len();
        let mut value = [0u8; 20];
        let value_len = value.len();

        if value_len > account_id_len {
            value[(value_len - account_id_len)..].copy_from_slice(account_id.as_ref());
        } else {
            value.copy_from_slice(&account_id.as_ref()[(account_id_len - value_len)..]);
        }

        H160::from(value)
    }
}

/// SSVM module trait
pub trait Trait: frame_system::Trait + pallet_timestamp::Trait {
    /// Convert account ID to H160;
    type ConvertAccountId: ConvertAccountId<Self::AccountId>;
    /// Currency type for deposit and withdraw.
    type Currency: Currency<Self::AccountId>;
    /// The overarching event type.
    type Event: From<Event> + Into<<Self as frame_system::Trait>::Event>;
}

decl_storage! {
    trait Store for Module<T: Trait> as SSVM {
        Accounts get(fn accounts) config(): map hasher(blake2_128_concat) H160 => Account;
        AccountCodes: map hasher(blake2_128_concat) H160 => Vec<u8>;
        AccountStorages: double_map hasher(blake2_128_concat) H160, hasher(blake2_128_concat) H256 => H256;
    }
}

decl_event! {
    /// SSVM events
    pub enum Event {
        Nonce(U256),
        Create(H160),
        Call(H160),
        Output(Vec<u8>),
        Log(Log),
        // LogMessage(String),
    }
}

decl_error! {
    pub enum Error for Module<T: Trait> {
        /// Not enough balance to perform action
        BalanceLow,
        /// Calculating total fee overflowed
        FeeOverflow,
        /// Calculating total payment overflowed
        PaymentOverflow,
        /// Withdraw fee failed
        WithdrawFailed,
        /// Gas price is too low.
        GasPriceTooLow,
        /// Call failed
        ExitReasonFailed,
        /// Call reverted
        ExitReasonRevert,
        /// Call returned VM fatal error
        ExitReasonFatal,
        /// Nonce is invalid
        InvalidNonce,
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        type Error = Error<T>;

        fn deposit_event() = default;

        /// Deposit balance from currency/balances module into Ewasm.
        #[weight = SimpleDispatchInfo::FixedNormal(10_000)]
        fn deposit_balance(origin, value: BalanceOf<T>) {
            let sender = ensure_signed(origin)?;

            let imbalance = T::Currency::withdraw(
                &sender,
                value,
                WithdrawReason::Reserve.into(),
                ExistenceRequirement::AllowDeath,
            )?;
            T::Currency::resolve_creating(&Self::account_id(), imbalance);

            let bvalue = U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(value));
            let address = T::ConvertAccountId::convert_account_id(&sender);
            Accounts::mutate(&address, |account| {
                account.balance += bvalue;
            });
        }

        /// Withdraw balance from Ewasm into currency/balances module.
        #[weight = SimpleDispatchInfo::FixedNormal(10_000)]
        fn withdraw_balance(origin, value: BalanceOf<T>) {
            let sender = ensure_signed(origin)?;
            let address = T::ConvertAccountId::convert_account_id(&sender);
            let bvalue = U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(value));

            let mut account = Accounts::get(&address);
            account.balance = account.balance.checked_sub(bvalue)
                .ok_or(Error::<T>::BalanceLow)?;

            let imbalance = T::Currency::withdraw(
                &Self::account_id(),
                value,
                WithdrawReason::Reserve.into(),
                ExistenceRequirement::AllowDeath
            )?;

            Accounts::insert(&address, account);

            T::Currency::resolve_creating(&sender, imbalance);
        }

        /// Issue an Ewasm call operation. This is similar to a message call transaction in Ethereum.
        #[weight = FunctionOf(|(_, _, _, gas_limit, gas_price): (&H160, &Vec<u8>, &U256, &u32, &U256)| (*gas_price).saturated_into::<Weight>().saturating_mul(*gas_limit), DispatchClass::Normal, true)]
        fn call(
            origin,
            target: H160,
            input: Vec<u8>,
            value: U256,
            gas_limit: u32,
            gas_price: U256,
        ) -> DispatchResult {
            if_std!{
                let sender = ensure_signed(origin)?;
                let source = T::ConvertAccountId::convert_account_id(&sender);
                let nonce = Accounts::get(&source).nonce;
                let (result, gas_left, status_code) = Self::execute_ssvm(
                    source,
                    target,
                    value,
                    input,
                    gas_limit,
                    gas_price,
                    nonce,
                    CallKind::EVMC_CALL,
                )?;

                if status_code == StatusCode::EVMC_SUCCESS {
                    Self::sync_storage();
                }
                Self::drop_storage_cache();

                Accounts::mutate(&source, |account| {
                    account.nonce += U256::one();
                });
                Module::<T>::deposit_event(Event::Call(target));
                Module::<T>::deposit_event(Event::Output(result.to_owned()));
            }
            Ok(())
        }

        /// Create contract with Ewasm
        #[weight = FunctionOf(|(_, _, gas_limit, gas_price): (&Vec<u8>, &U256, &u32, &U256)| (*gas_price).saturated_into::<Weight>().saturating_mul(*gas_limit), DispatchClass::Normal, true)]
        fn create(
            origin,
            code: Vec<u8>,
            value: U256,
            gas_limit: u32,
            gas_price: U256,
        ) -> DispatchResult {
            if_std!{
                let sender = ensure_signed(origin)?;
                let source = T::ConvertAccountId::convert_account_id(&sender);
                let nonce = Accounts::get(&source).nonce;
                let created_address = create_address(source, nonce);
                let (output, gas_left, status_code) = Self::execute_ssvm(
                    source,
                    created_address,
                    value,
                    code,
                    gas_limit,
                    gas_price,
                    nonce,
                    CallKind::EVMC_CREATE,
                )?;

                if status_code == StatusCode::EVMC_SUCCESS {
                    Self::sync_storage();
                }
                Self::drop_storage_cache();

                Accounts::mutate(&source, |account| {
                    account.nonce += U256::one();
                });
                AccountCodes::insert(created_address, output.to_owned());
                Module::<T>::deposit_event(Event::Create(created_address));
            }
            Ok(())
        }
    }
}

#[cfg(feature = "std")]
lazy_static::lazy_static! {
    static ref STORAGE_CACHE:Mutex<HashMap<(H160, H256), H256>> = Mutex::new(HashMap::new());
}

impl<T: Trait> Module<T> {
    /// The account ID of the SSVM module.
    pub fn account_id() -> T::AccountId {
        MODULE_ID.into_account()
    }

    /// Check whether an account is empty.
    pub fn is_account_empty(address: &H160) -> bool {
        let account = Accounts::get(address);
        let code_len = AccountCodes::decode_len(address).unwrap_or(0);

        account.nonce == U256::zero() && account.balance == U256::zero() && code_len == 0
    }

    /// Remove an account if its empty.
    pub fn remove_account_if_empty(address: &H160) {
        if Self::is_account_empty(address) {
            Self::remove_account(address)
        }
    }

    /// Remove an account from state.
    fn remove_account(address: &H160) {
        Accounts::remove(address);
        AccountCodes::remove(address);
        AccountStorages::remove_prefix(address);
    }

    /// Get storage value through storage cache
    #[cfg(feature = "std")]
    fn get_storage(address: H160, key: H256) -> H256 {
        if !STORAGE_CACHE.lock().unwrap().contains_key(&(address, key)) {
            let value = AccountStorages::get(H160::from(address), H256::from(key));
            STORAGE_CACHE.lock().unwrap().insert((address, key), value);
        }
        STORAGE_CACHE
            .lock()
            .unwrap()
            .get(&(address, key))
            .unwrap()
            .to_owned()
    }

    /// Set storage value to storage cache
    #[cfg(feature = "std")]
    fn set_storage(address: H160, key: H256, value: H256) -> StorageStatus {
        STORAGE_CACHE.lock().unwrap().insert((address, key), value);
        StorageStatus::EVMC_STORAGE_MODIFIED
    }

    /// Drop storage cache
    #[cfg(feature = "std")]
    fn drop_storage_cache() {
        STORAGE_CACHE.lock().unwrap().clear();
    }

    /// Sync storage cache to storage
    #[cfg(feature = "std")]
    fn sync_storage() {
        for (key, val) in STORAGE_CACHE.lock().unwrap().iter() {
            AccountStorages::insert(key.0, key.1, val);
        }
    }

    /// Execute precompiles contract.
    #[cfg(feature = "std")]
    fn execute_precompiles(
        target: &H160,
        value: &U256,
        data: &Vec<u8>,
        gas_limit: &u32,
        gas_price: &U256,
    ) -> (bool, Vec<u8>, i64) {
        match &hex::encode(target)[..] {
            "0000000000000000000000000000000000000002" => {
                return (true, Sha256::digest(&data).to_vec(), *gas_limit as i64);
            }
            "0000000000000000000000000000000000000009" => {
                return (true, Keccak256::digest(&data).to_vec(), *gas_limit as i64);
            }
            _ => {
                return (false, vec![0u8], *gas_limit as i64);
            }
        }
    }

    /// Execute SSVM.
    #[cfg(feature = "std")]
    fn execute_ssvm(
        source: H160,
        target: H160,
        value: U256,
        data: Vec<u8>,
        gas_limit: u32,
        gas_price: U256,
        nonce: U256,
        call_kind: CallKind,
    ) -> Result<(Vec<u8>, i64, StatusCode), Error<T>> {
        // No coinbase, difficulty in substrate nodes.
        let coinbase = H160::zero();
        let difficulty = U256::zero();
        let chain_id = U256::from(sp_io::misc::chain_id());
        let block_number: u128 = frame_system::Module::<T>::block_number().unique_saturated_into();
        let timestamp: u128 = pallet_timestamp::Module::<T>::get().unique_saturated_into();

        let (is_precompiles, output, gas_left) =
            Self::execute_precompiles(&target, &value, &data, &gas_limit, &gas_price);
        if is_precompiles {
            return Ok((output.to_vec(), gas_left, StatusCode::EVMC_SUCCESS));
        }

        let code = match call_kind {
            CallKind::EVMC_CALL => AccountCodes::get(&target),
            CallKind::EVMC_CREATE => data.to_owned(),
            _ => data.to_owned(),
        };
        let tx_context = TxContext::new(
            gas_price,
            source,
            coinbase,
            block_number.try_into().unwrap(),
            timestamp.try_into().unwrap(),
            gas_limit.into(),
            difficulty,
            chain_id,
        );
        let context = HostContext::<T>::new(tx_context);
        let depth = 0;
        let create2_salt = [0u8; 32];
        let vm = ssvm::create();
        let (output, gas_left, status_code) = vm.execute(
            Box::new(context),
            Revision::EVMC_BYZANTIUM,
            call_kind,
            false,
            depth,
            gas_limit.into(),
            target.as_fixed_bytes(),
            source.as_fixed_bytes(),
            &data[..],
            &value.into(),
            &code,
            &create2_salt,
        );
        return Ok((output.to_vec(), gas_left, status_code));
    }
}
