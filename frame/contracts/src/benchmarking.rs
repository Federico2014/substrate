// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
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

//! Benchmarks for the contracts pallet

#![cfg(feature = "runtime-benchmarks")]

extern crate alloc;

use crate::*;
use crate::Module as Contracts;

use frame_system::RawOrigin;
use frame_benchmarking::{benchmarks, account};
use sp_runtime::traits::{Bounded, Hash};
use alloc::string::String;

const SEED: u32 = 0;

macro_rules! load_module {
    ($name:expr) => {{
        let code = include_bytes!(concat!("../fixtures/benchmarks/", $name, ".wat"));
        compile_module::<T>(code)
    }};
}

fn compile_module<T: Trait>(code: &[u8]) -> (Vec<u8>, <T::Hashing as Hash>::Output) {
    let code = sp_std::str::from_utf8(code).expect("Invalid utf8 in wat file.");
    compile_code::<T>(code)
}

fn compile_code<T: Trait>(code: &str) -> (Vec<u8>, <T::Hashing as Hash>::Output) {
    let binary = wat::parse_str(code).expect("Failed to compile wat file.");
    let hash = T::Hashing::hash(&binary);
    (binary, hash)
}

fn create_max_funded_user<T: Trait>(string: &'static str, n: u32) -> T::AccountId {
	let user = account(string, n, SEED);
	T::Currency::make_free_balance_be(&user, BalanceOf::<T>::max_value());
	user
}

fn expanded_contract<T: Trait>(expansions: u32) -> (Vec<u8>, <T::Hashing as Hash>::Output) {
    const CONTRACT_START: &str = r#"
        (module
            (func (export "deploy"))
            (func (export "call")

    "#;
    const CONTRACT_EXPANSION: &str = "( if (i32.const 0) (then (return)) )";
    const CONTRACT_END: &str = "))";
    let expansion_len = CONTRACT_EXPANSION.len() * expansions as usize;
    let len = CONTRACT_START.len() + expansion_len + CONTRACT_END.len();
    let mut contract = String::with_capacity(len);
    contract.push_str(CONTRACT_START);
    for _ in 1 .. expansions {
        contract.push_str(CONTRACT_EXPANSION);
    }
    contract.push_str(CONTRACT_END);
    compile_code::<T>(&contract)
}

benchmarks! {
    _ {
    }

    put_code {
        let n in 0 .. 2_000;
        let caller = create_max_funded_user::<T>("caller", 0);
        let (binary, hash) = expanded_contract::<T>(n);
    }: _(RawOrigin::Signed(caller), binary)

    instantiate {
        // The size of the data has no influence on the costs of this extrinsic
        // as long as the contract won't call `ext_input` to copy the data to contract
        // memory. The dummy contract used here does not do this. The costs for the
        // data copy is billed as part of `ext_input`.
        let data = vec![0u8; 128];
        let endowment = T::Currency::minimum_balance();
        let caller = create_max_funded_user::<T>("caller", 0);
        let (binary, hash) = load_module!("dummy");
        Contracts::<T>::put_code(RawOrigin::Signed(caller.clone()).into(), binary.to_vec())
            .unwrap();

    }: _(
            RawOrigin::Signed(caller.clone()),
            endowment,
            Weight::max_value(),
            hash,
            data
        )
    verify {
        assert_eq!(
            BalanceOf::<T>::max_value() - endowment,
            T::Currency::free_balance(&caller),
        )
    }

    call {
        // Same argument as for instantiate.
        let data = vec![0u8; 128];
        let endowment = T::Currency::minimum_balance() * 1_000.into();
        let value = T::Currency::minimum_balance() * 100.into();
        let caller = create_max_funded_user::<T>("caller", 0);
        let (binary, hash) = load_module!("dummy");
        let addr = T::DetermineContractAddress::contract_address_for(&hash, &data, &caller);
        Contracts::<T>::put_code(RawOrigin::Signed(caller.clone()).into(), binary.to_vec())
            .unwrap();
        Contracts::<T>::instantiate(
            RawOrigin::Signed(caller.clone()).into(),
            endowment,
            Weight::max_value(),
            hash,
            vec![],
        ).unwrap();
    }: _(
            RawOrigin::Signed(caller.clone()),
            T::Lookup::unlookup(addr),
            value,
            Weight::max_value(),
            data
        )
    verify {
        assert_eq!(
            BalanceOf::<T>::max_value() - endowment - value,
            T::Currency::free_balance(&caller),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{ExtBuilder, Test};
    use frame_support::assert_ok;

    #[test]
    fn put_code() {
		ExtBuilder::default().build().execute_with(|| {
			assert_ok!(test_benchmark_put_code::<Test>());
		});
    }

    #[test]
    fn instantiate() {
		ExtBuilder::default().build().execute_with(|| {
			assert_ok!(test_benchmark_instantiate::<Test>());
		});
    }

    #[test]
    fn call() {
		ExtBuilder::default().build().execute_with(|| {
			assert_ok!(test_benchmark_call::<Test>());
		});
	}
}
