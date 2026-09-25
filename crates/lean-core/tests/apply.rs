//! `apply` (shard-parallel for large address-sorted write sets) against the
//! definition — `insert` every write in order — on large sorted, unsorted and
//! duplicate-carrying deltas; and the sharded `AccountMap` against a plain
//! `HashMap`.
use alloy_primitives::Address;
use lean_core::{apply, AccountMap, Acct, Delta, FlatState};
use std::collections::HashMap;

fn addr(i: u64) -> Address {
    let mut x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5eed;
    let mut b = [0u8; 20];
    for c in b.chunks_mut(4) {
        x ^= x >> 29;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 32;
        c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
    }
    Address::from(b)
}

fn acct(i: u64) -> Acct {
    Acct {
        nonce: i % 7,
        balance: i as u128 * 1_000_000_007,
    }
}

fn check(pre_n: u64, writes: Vec<(Address, Acct)>) {
    let mut state = FlatState::default();
    let mut want: HashMap<Address, Acct> = HashMap::new();
    for i in 0..pre_n {
        state.accounts.insert(addr(i), acct(i));
        want.insert(addr(i), acct(i));
    }
    for (a, x) in &writes {
        want.insert(*a, *x);
    }
    let delta = Delta {
        writes,
        ..Default::default()
    };
    apply(&mut state, &delta);
    assert_eq!(state.accounts.len(), want.len());
    for (a, x) in &want {
        assert_eq!(state.accounts.get(a), Some(x));
    }
    let got: HashMap<Address, Acct> = state.accounts.iter().map(|(a, x)| (*a, *x)).collect();
    assert_eq!(got, want);
}

#[test]
fn apply_matches_insert_in_order() {
    // sorted, large (parallel path): updates + new accounts
    let mut w: Vec<(Address, Acct)> = (0..20_000u64)
        .map(|i| (addr(i * 3), acct(i + 100)))
        .collect();
    w.sort_by_key(|(a, _)| *a);
    check(30_000, w.clone());
    // unsorted, large (serial fallback)
    let mut u = w.clone();
    u.reverse();
    check(30_000, u);
    // sorted with repeated addresses: last write wins
    let mut d = w.clone();
    for i in 0..500 {
        let (a, _) = w[i * 7];
        d.push((a, acct(999_999 + i as u64)));
    }
    d.sort_by_key(|(a, _)| *a); // stable: the later value stays later
    check(30_000, d);
    // small (serial)
    check(10, w[..50].to_vec());
    check(0, vec![]);
}

#[test]
fn account_map_behaves_like_a_hashmap() {
    let mut m = AccountMap::with_capacity(1000);
    let mut h: HashMap<Address, Acct> = HashMap::new();
    for i in 0..5_000u64 {
        let a = addr(i % 3_000);
        assert_eq!(m.insert(a, acct(i)), h.insert(a, acct(i)));
        if i % 11 == 0 {
            let r = addr(i / 2);
            assert_eq!(m.remove(&r), h.remove(&r));
        }
    }
    assert_eq!(m.len(), h.len());
    assert!(!m.is_empty());
    for i in 0..4_000u64 {
        let a = addr(i);
        assert_eq!(m.get(&a), h.get(&a));
        assert_eq!(m.contains_key(&a), h.contains_key(&a));
    }
    if let Some(x) = m.get_mut(&addr(1)) {
        x.nonce = 42;
    }
    if let Some(x) = h.get_mut(&addr(1)) {
        x.nonce = 42;
    }
    let got: HashMap<Address, Acct> = (&m).into_iter().map(|(a, x)| (*a, *x)).collect();
    assert_eq!(got, h);
    m.reserve(10_000);
    assert_eq!(m.len(), h.len());
    assert!(AccountMap::new().is_empty());
}
