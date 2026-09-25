//! SPEC.md Appendix A — every vector, reproduced exactly (chain id 1338).
//! These are ground truth produced by the real engine.

use lean_oracle::primitives::SECP256K1_N;
use lean_oracle::sign::{address_of, sign_tx, small_secret};
use lean_oracle::*;

const CHAIN: u64 = 1338;

fn h(s: &str) -> Vec<u8> {
    hex::decode(s.trim_start_matches("0x")).unwrap()
}
fn addr(s: &str) -> Address {
    h(s).try_into().unwrap()
}
fn rep(b: u8) -> Address {
    [b; 20]
}
fn lc(s: &str) -> String {
    s.trim_start_matches("0x").to_lowercase()
}

const T0_HEX: &str = "0x5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f84000";
const T1_HEX: &str = "0x500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601";
const T2_HEX: &str = "0x5000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701";
const K1: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const K2: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
const K3: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
const K9: &str = "0xF7Edc8FA1eCc32967F827C9043FcAe6ba73afA5c";

fn t0() -> Tx {
    sign_tx(
        &small_secret(1),
        0,
        vec![
            Output {
                to: rep(0x11),
                amount: 5,
            },
            Output {
                to: rep(0x22),
                amount: 1_000_000_000,
            },
        ],
        CHAIN,
    )
}
fn t1() -> Tx {
    sign_tx(
        &small_secret(2),
        0,
        vec![
            Output {
                to: addr(K3),
                amount: 2_000_000_000,
            },
            Output {
                to: rep(0x22),
                amount: 3,
            },
        ],
        CHAIN,
    )
}
fn t2() -> Tx {
    sign_tx(
        &small_secret(3),
        0,
        vec![Output {
            to: rep(0x33),
            amount: 10_500_000_000,
        }],
        CHAIN,
    )
}

/// V3 pre-state: k1, k2, k3 each (nonce 0, balance 1e19), built through §3.2.
fn v3_pre_state() -> State {
    genesis_state(&format!("{K1}\n{K2}\n{K3}\n"), 10_000_000_000_000_000_000).unwrap()
}

const V3_TS: u64 = 1_700_000_000_000;

fn v3_header() -> Header {
    Header {
        parent: genesis_commitment(CHAIN),
        number: 1,
        timestamp_ms: V3_TS,
    }
}

// ---------------------------------------------------------------- V1

#[test]
fn v1_domains_and_constants() {
    assert_eq!(
        hex::encode(lane_domain(1338)),
        "d836bb4e7944843ac5be0c4094de2459090af9911964b0d4f9d1eaab9e79f3d5"
    );
    assert_eq!(
        hex::encode(genesis_commitment(1338)),
        "ef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e5770"
    );
    assert_eq!(
        hex::encode(genesis_commitment(1337)),
        "255809a2b8fb46ec0cc08f722c443211a405366c5fbb0eb700dcbae3c0b3cad9"
    );
    assert_eq!(
        hex::encode(state_digest(&State::new())),
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    );
    assert_eq!(address_of(&small_secret(1)), addr(K1));
    assert_eq!(address_of(&small_secret(2)), addr(K2));
    assert_eq!(address_of(&small_secret(3)), addr(K3));
    assert_eq!(address_of(&small_secret(9)), addr(K9));
}

// ---------------------------------------------------------------- V2

#[test]
fn v2_signed_tx() {
    let tx = t0();
    let bytes = tx.encode();
    assert_eq!(bytes.len(), 128);
    assert_eq!(tx.body().len(), 63);
    assert_eq!(bytes, h(T0_HEX));
    assert_eq!(
        hex::encode(signing_hash(&tx, CHAIN)),
        "486c80d3a44ff800c886164aa71a32af1878e3453849f9f5289e3eb250369095"
    );
    assert_eq!(
        hex::encode(tx_hash(&bytes)),
        "36964dc5ce2098e75107e8badf4c4f29f17d476e10c1e946eeda83a3ffd3c427"
    );

    // Decoding the vector bytes (not our own signature) gives the same tx and sender.
    let decoded = decode_tx(&h(T0_HEX)).unwrap();
    assert_eq!(decoded, tx);
    assert_eq!(decoded.encode(), h(T0_HEX));
    assert_eq!(recover_sender(&decoded, CHAIN).unwrap(), addr(K1));
    // The chain id is bound through the domain: another chain recovers someone else.
    assert_ne!(recover_sender(&decoded, 1337).ok(), Some(addr(K1)));
}

// ---------------------------------------------------------------- V3

#[test]
fn v3_tx_bytes_and_hashes() {
    assert_eq!(t1().encode(), h(T1_HEX));
    assert_eq!(t2().encode(), h(T2_HEX));
    assert_eq!(
        hex::encode(tx_hash(&h(T1_HEX))),
        "e281aa4b4defd5f63af5f0e4ed2bf397de26ce79520504f352c838d083328362"
    );
    assert_eq!(
        hex::encode(tx_hash(&h(T2_HEX))),
        "83133719eeb10dea1ebc2a45e93d531efa119418606bcce17caf2b508133c359"
    );
    assert_eq!(
        recover_sender(&decode_tx(&h(T1_HEX)).unwrap(), CHAIN),
        Ok(addr(K2))
    );
    assert_eq!(
        recover_sender(&decode_tx(&h(T2_HEX)).unwrap(), CHAIN),
        Ok(addr(K3))
    );
}

const V3_WIRE: &str = "0xef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e577001000000000000000068e5cf8b01000003000000800000005000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f8400080000000500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601640000005000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701";
const V3_COMMITMENT: &str = "095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b";
const V3_POST_DIGEST: &str = "a17fd987cd27f9aed1cb9704025a008371d93ecfe1191dbdb9d569f03f7f3f8a";

#[test]
fn v3_block_wire_and_commitment() {
    let entries = vec![h(T0_HEX), h(T1_HEX), h(T2_HEX)];
    let wire = encode_block(&v3_header(), &entries);
    assert_eq!(wire.len(), 420);
    assert_eq!(wire, h(V3_WIRE));

    let block = decode_block(&h(V3_WIRE)).unwrap();
    assert_eq!(block.header, v3_header());
    assert_eq!(block.entries, entries);
    let th = txs_hash(&block.entries);
    assert_eq!(
        hex::encode(th),
        "020d1e85fb0e4ed45ebee17a6266b7f4b0d2ab04f0f4c1ed00f36016d7f2149d"
    );
    assert_eq!(th, keccak256(&h(V3_WIRE)[48..]));
    assert_eq!(hex::encode(commitment(&block.header, &th)), V3_COMMITMENT);
}

#[test]
fn v3_block_execution() {
    let mut state = v3_pre_state();
    assert_eq!(
        hex::encode(state_digest(&state)),
        "c1f91687aba143b488b31ddfb40fb4c845583df50cc305a30f1baf0446087ea6"
    );
    let out = apply_block(&mut state, &h(V3_WIRE), CHAIN).unwrap();
    assert_eq!(
        (out.applied, out.noops, out.outputs, out.fees_to_beneficiary),
        (2, 1, 4, 62_000_000_000_000)
    );
    assert_eq!((out.items, out.skipped), (3, 0));

    let expected: Vec<(Address, u64, u128)> = vec![
        (BENEFICIARY, 0, 62_000_000_000_000),
        (rep(0x11), 0, 5_000_000_000),
        (rep(0x22), 0, 1_000_000_003_000_000_000),
        (addr(K2), 1, 7_999_968_997_000_000_000),
        (addr(K3), 0, 12_000_000_000_000_000_000),
        (addr(K1), 1, 8_999_968_995_000_000_000),
    ];
    let got: Vec<(Address, u64, u128)> = state
        .accounts
        .iter()
        .map(|(a, acct)| (*a, acct.nonce, acct.balance))
        .collect();
    assert_eq!(got, expected); // also checks sorted order and that 0x33… is absent
    assert!(!state.contains(&rep(0x33)));
    assert_eq!(hex::encode(state_digest(&state)), V3_POST_DIGEST);
}

#[test]
fn v3_through_chain_append() {
    let mut chain = Chain::genesis(CHAIN, v3_pre_state());
    chain.append(&h(V3_WIRE)).unwrap();
    assert_eq!(hex::encode(chain.head.commitment), V3_COMMITMENT);
    assert_eq!(chain.head.number, 1);
    assert_eq!(hex::encode(state_digest(&chain.state)), V3_POST_DIGEST);
}

// ---------------------------------------------------------------- V4

const V4_WIRE: &str = "0x095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b0200000000000000f469e5cf8b01000000000000";

#[test]
fn v4_empty_blocks() {
    assert_eq!(
        hex::encode(txs_hash(&[])),
        "e8e77626586f73b955364c7b4bbf0bb7f7685ebd40e852b164633a4acbd3244c"
    );
    assert_eq!(txs_hash(&[]), keccak256(&[0, 0, 0, 0]));

    let hdr = Header {
        parent: h(V3_COMMITMENT).try_into().unwrap(),
        number: 2,
        timestamp_ms: 1_700_000_000_500,
    };
    let wire = encode_block(&hdr, &[]);
    assert_eq!(wire, h(V4_WIRE));
    assert_eq!(
        hex::encode(decode_block(&wire).unwrap().commitment()),
        "0d99c95bba358956556d8b60084682c479195007ad0ef8792acad9746e205a40"
    );

    // Chained after V3: the empty block re-writes the beneficiary with +0 ⇒ digest unchanged.
    let mut chain = Chain::genesis(CHAIN, v3_pre_state());
    chain.append(&h(V3_WIRE)).unwrap();
    let out = chain.append(&wire).unwrap();
    assert_eq!(out, Outcome::default());
    assert_eq!(
        hex::encode(chain.head.commitment),
        "0d99c95bba358956556d8b60084682c479195007ad0ef8792acad9746e205a40"
    );
    assert_eq!(hex::encode(state_digest(&chain.state)), V3_POST_DIGEST);

    // An empty block on the EMPTY state writes 0x…be = (0,0).
    let mut empty = State::new();
    apply_block(&mut empty, &wire, CHAIN).unwrap();
    assert_eq!(empty.len(), 1);
    assert_eq!(empty.get(&BENEFICIARY), Account::default());
    assert_eq!(
        hex::encode(state_digest(&empty)),
        "6e1bd6f9532cfbf5580724858fcedc7089a23d8307c0594ef5dd150f4c1fda9f"
    );
}

// ---------------------------------------------------------------- V5

const V5_HEX: &str = "0x5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f9d3dfd988e63f11eea1c42ea53aaf69b61c4e33b3d611f4ea06ae7a9a37a3490101";

/// n − s, big-endian.
fn neg_mod_n(s: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = i16::from(SECP256K1_N[i]) - i16::from(s[i]) - borrow;
        borrow = 0;
        if d < 0 {
            d += 256;
            borrow = 1;
        }
        out[i] = d as u8;
    }
    assert_eq!(borrow, 0);
    out
}

#[test]
fn v5_high_s_twin() {
    let mut twin = t0();
    twin.s = neg_mod_n(&twin.s);
    twin.v ^= 1;
    assert_eq!(twin.v, 1);
    assert_eq!(twin.encode(), h(V5_HEX));
    assert_eq!(
        hex::encode(tx_hash(&h(V5_HEX))),
        "5b3580c6a86476b5c39c7c368793b22bf7299b3ad51257c73e6be202fe8b340c"
    );
    let decoded = decode_tx(&h(V5_HEX)).unwrap();
    assert_eq!(recover_sender(&decoded, CHAIN), Ok(addr(K1)));

    // Accepted in a block: the twin replaces T0 in V3 and the post-state is identical
    // (tx hash affects no state, §1.6).
    let mut state = v3_pre_state();
    let wire = encode_block(&v3_header(), &[h(V5_HEX), h(T1_HEX), h(T2_HEX)]);
    let out = apply_block(&mut state, &wire, CHAIN).unwrap();
    assert_eq!((out.applied, out.noops), (2, 1));
    assert_eq!(hex::encode(state_digest(&state)), V3_POST_DIGEST);

    // Only one of the twins can apply (same nonce).
    let mut state = v3_pre_state();
    let wire = encode_block(&v3_header(), &[h(T0_HEX), h(V5_HEX)]);
    let out = apply_block(&mut state, &wire, CHAIN).unwrap();
    assert_eq!((out.applied, out.noops), (1, 1));
}

// ---------------------------------------------------------------- V6

#[test]
fn v6_v_byte() {
    for v in [2u8, 3, 4, 27, 28] {
        let mut b = h(T0_HEX);
        *b.last_mut().unwrap() = v;
        let tx = decode_tx(&b).unwrap(); // decode is fine; recovery fails
        assert!(recover_sender(&tx, CHAIN).is_err(), "v = {v}");
        assert!(classify_entry(&b, CHAIN).is_none(), "v = {v}");

        // In a block the entry is skipped: sender k1 is not written.
        let mut state = v3_pre_state();
        let before = state.get(&addr(K1));
        let wire = encode_block(&v3_header(), &[b]);
        let out = apply_block(&mut state, &wire, CHAIN).unwrap();
        assert_eq!(
            (out.items, out.skipped, out.applied, out.noops),
            (0, 1, 0, 0)
        );
        assert_eq!(state.get(&addr(K1)), before);
    }
}

// ---------------------------------------------------------------- V7

#[test]
fn v7_trailing_bytes() {
    let mut entry = h(T0_HEX);
    entry.extend_from_slice(&[0xde, 0xad]);
    assert_eq!(entry.len(), 130);
    assert_eq!(
        decode_tx(&entry),
        Err(Malformed::TrailingBytes {
            expected: 128,
            got: 130
        })
    );
    let item = classify_entry(&entry, CHAIN).expect("lean tx as a block entry");
    assert_eq!(item.sender, addr(K1));
    assert_eq!(
        hex::encode(item.tx_hash),
        "36964dc5ce2098e75107e8badf4c4f29f17d476e10c1e946eeda83a3ffd3c427"
    );

    // The block commitment covers all 130 bytes.
    let with_tail = encode_block(&v3_header(), std::slice::from_ref(&entry));
    let without = encode_block(&v3_header(), &[h(T0_HEX)]);
    let b1 = decode_block(&with_tail).unwrap();
    let b2 = decode_block(&without).unwrap();
    assert_eq!(b1.entries[0].len(), 130);
    assert_eq!(b1.txs_hash(), keccak256(&with_tail[48..]));
    assert_ne!(b1.commitment(), b2.commitment());

    // …and it executes exactly like the bare tx.
    let mut s1 = v3_pre_state();
    let mut s2 = v3_pre_state();
    apply_block(&mut s1, &with_tail, CHAIN).unwrap();
    apply_block(&mut s2, &without, CHAIN).unwrap();
    assert_eq!(state_digest(&s1), state_digest(&s2));
    assert_eq!(s1.get(&addr(K1)).nonce, 1);
}

// ---------------------------------------------------------------- V8

#[test]
fn v8_skipped_vs_noop() {
    let t9 = sign_tx(
        &small_secret(9),
        0,
        vec![Output {
            to: rep(0x44),
            amount: 1,
        }],
        CHAIN,
    );
    let entries = vec![vec![], h("500000"), h("0201"), t9.encode()];
    let hdr = Header {
        parent: genesis_commitment(CHAIN),
        number: 1,
        timestamp_ms: 0,
    };
    let wire = encode_block(&hdr, &entries);
    assert_eq!(
        hex::encode(decode_block(&wire).unwrap().commitment()),
        "f89650c46f7da024f15e6c5530d66bd7e076b350674a808df40f9139ab58f18c"
    );

    let mut state = State::new();
    let out = apply_block(&mut state, &wire, CHAIN).unwrap();
    assert_eq!(out.items, 1);
    assert_eq!(out.skipped, 3);
    assert_eq!(
        (out.applied, out.noops, out.outputs, out.fees_to_beneficiary),
        (0, 1, 0, 0)
    );
    let got: Vec<(Address, Account)> = state.accounts.clone().into_iter().collect();
    assert_eq!(
        got,
        vec![
            (BENEFICIARY, Account::default()),
            (addr(K9), Account::default())
        ]
    );
    assert!(!state.contains(&rep(0x44)));
    assert_eq!(
        hex::encode(state_digest(&state)),
        "162235580f8292ac3bf1e0091420f8a16d448afa47cd4a6c687b8c7061285fff"
    );
    // Sanity: address constants in the spec are the same bytes in any case.
    assert_eq!(lc(K9), hex::encode(addr(K9)));
}
