//! SPEC.md Appendix A — every vector asserted against the ENGINE (lean-core +
//! the lean-native codec/signer it uses), under both recovery modes. The
//! oracle has its own copy of these in `crates/lean-oracle/tests/vectors.rs`.

use crate::util::{beneficiary, ctx, domain, engine_genesis, high_s_twin, Key};
use alloy_primitives::{keccak256, Address, B256};
use lean_core::{framing, BlockHeader, FlatState};
use lean_native::{lane_domain, LeanTx, Output};

const T0_HEX: &str = "5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f84000";
const T1_HEX: &str = "500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601";
const T2_HEX: &str = "5000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701";
const V3_WIRE: &str = "ef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e577001000000000000000068e5cf8b01000003000000800000005000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f8400080000000500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601640000005000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701";
const V4_WIRE: &str = "095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b0200000000000000f469e5cf8b01000000000000";
const V5_HEX: &str = "5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f9d3dfd988e63f11eea1c42ea53aaf69b61c4e33b3d611f4ea06ae7a9a37a3490101";

const K1: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const K2: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
const K3: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
const K9: &str = "0xF7Edc8FA1eCc32967F827C9043FcAe6ba73afA5c";

fn h(s: &str) -> Vec<u8> {
    hex::decode(s.trim_start_matches("0x")).expect("vector hex")
}
fn b256(s: &str) -> B256 {
    B256::from_slice(&h(s))
}
fn addr(s: &str) -> Address {
    s.parse().expect("vector address")
}
fn rep(b: u8) -> Address {
    Address::from([b; 20])
}
fn small_key(k: u8) -> Key {
    let mut s = [0u8; 32];
    s[31] = k;
    Key::from_secret(s).expect("small key")
}
fn out(to: Address, amount: u64) -> Output {
    Output { to, amount }
}

macro_rules! check {
    ($n:expr, $cond:expr, $($fmt:tt)*) => {{
        *$n += 1;
        if !$cond {
            return Err(format!($($fmt)*));
        }
    }};
}

fn t0() -> LeanTx {
    small_key(1).sign(0, vec![out(rep(0x11), 5), out(rep(0x22), 1_000_000_000)])
}

/// Post-state as `(address, nonce, balance)` sorted by address.
fn sorted(state: &FlatState) -> Vec<(Address, u64, u128)> {
    let mut v: Vec<_> = state
        .accounts
        .iter()
        .map(|(a, x)| (*a, x.nonce, x.balance))
        .collect();
    v.sort();
    v
}

/// Assert every vector; returns the number of assertions made.
pub fn check_all() -> Result<usize, String> {
    let mut n = 0usize;
    let n = &mut n;

    // ---- V1 ----
    check!(
        n,
        lane_domain(1338)
            == b256("0xd836bb4e7944843ac5be0c4094de2459090af9911964b0d4f9d1eaab9e79f3d5"),
        "V1 lane_domain(1338) = {}",
        lane_domain(1338)
    );
    for (cid, want) in [
        (
            1338u64,
            "0xef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e5770",
        ),
        (
            1337,
            "0x255809a2b8fb46ec0cc08f722c443211a405366c5fbb0eb700dcbae3c0b3cad9",
        ),
    ] {
        let g = lean_core::genesis_commitment(cid);
        check!(n, g == b256(want), "V1 GENESIS_COMMITMENT({cid}) = {g}");
    }
    let empty = FlatState::default().digest();
    check!(
        n,
        empty == b256("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"),
        "V1 digest(empty) = {empty}"
    );
    for (k, want) in [(1u8, K1), (2, K2), (3, K3), (9, K9)] {
        let a = small_key(k).addr;
        check!(n, a == addr(want), "V1 addr(k{k}) = {a}");
    }

    // ---- V2 ----
    let tx0 = t0();
    let t0b = tx0.encode();
    check!(n, t0b == h(T0_HEX), "V2 T0 bytes = 0x{}", hex::encode(&t0b));
    check!(n, t0b.len() == 128, "V2 T0 length {}", t0b.len());
    let sh = tx0.signing_hash(&domain());
    check!(
        n,
        sh == b256("0x486c80d3a44ff800c886164aa71a32af1878e3453849f9f5289e3eb250369095"),
        "V2 signing_hash = {sh}"
    );
    let sender = tx0
        .recover_sender(&domain())
        .map_err(|e| format!("V2 recover: {e:?}"))?;
    check!(n, sender == addr(K1), "V2 sender = {sender}");
    let t0_hash = b256("0x36964dc5ce2098e75107e8badf4c4f29f17d476e10c1e946eeda83a3ffd3c427");
    check!(n, keccak256(&t0b) == t0_hash, "V2 tx_hash");
    check!(
        n,
        LeanTx::decode(&t0b).as_ref() == Ok(&tx0),
        "V2 strict decode round-trip"
    );

    // ---- V3 ----
    let tx1 = small_key(2).sign(0, vec![out(addr(K3), 2_000_000_000), out(rep(0x22), 3)]);
    let tx2 = small_key(3).sign(0, vec![out(rep(0x33), 10_500_000_000)]);
    check!(
        n,
        tx1.encode() == h(T1_HEX),
        "V3 T1 bytes = 0x{}",
        hex::encode(tx1.encode())
    );
    check!(
        n,
        tx2.encode() == h(T2_HEX),
        "V3 T2 bytes = 0x{}",
        hex::encode(tx2.encode())
    );
    check!(
        n,
        keccak256(tx1.encode())
            == b256("0xe281aa4b4defd5f63af5f0e4ed2bf397de26ce79520504f352c838d083328362"),
        "V3 T1 hash"
    );
    check!(
        n,
        keccak256(tx2.encode())
            == b256("0x83133719eeb10dea1ebc2a45e93d531efa119418606bcce17caf2b508133c359"),
        "V3 T2 hash"
    );
    let pre = engine_genesis(&format!("{K1}\n{K2}\n{K3}\n"), 10_000_000_000_000_000_000)?;
    check!(
        n,
        pre.digest() == b256("0xc1f91687aba143b488b31ddfb40fb4c845583df50cc305a30f1baf0446087ea6"),
        "V3 pre-digest = {}",
        pre.digest()
    );
    let hdr1 = BlockHeader {
        parent: lean_core::genesis_commitment(1338),
        number: 1,
        timestamp_ms: 1_700_000_000_000,
    };
    let wire3 = framing::to_wire(&hdr1, &[tx0.encode(), tx1.encode(), tx2.encode()]);
    check!(
        n,
        wire3 == h(V3_WIRE),
        "V3 wire = 0x{}",
        hex::encode(&wire3)
    );
    check!(n, wire3.len() == 420, "V3 wire length {}", wire3.len());
    check!(
        n,
        keccak256(&wire3[48..])
            == b256("0x020d1e85fb0e4ed45ebee17a6266b7f4b0d2ab04f0f4c1ed00f36016d7f2149d"),
        "V3 txs_hash"
    );
    let v3_commit = b256("0x095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b");
    let v3_post: Vec<(Address, u64, u128)> = vec![
        (beneficiary(), 0, 62_000_000_000_000),
        (rep(0x11), 0, 5_000_000_000),
        (rep(0x22), 0, 1_000_000_003_000_000_000),
        (addr(K2), 1, 7_999_968_997_000_000_000),
        (addr(K3), 0, 12_000_000_000_000_000_000),
        (addr(K1), 1, 8_999_968_995_000_000_000),
    ];
    let mut v3_post = v3_post;
    v3_post.sort();
    let v3_digest = b256("0xa17fd987cd27f9aed1cb9704025a008371d93ecfe1191dbdb9d569f03f7f3f8a");
    for par in [true, false] {
        let p = lean_core::process_block(&pre, &wire3, &ctx(par))
            .map_err(|e| format!("V3 process_block(par={par}): {e}"))?;
        check!(n, p.header == hdr1, "V3 header (par={par})");
        check!(
            n,
            p.commitment == v3_commit,
            "V3 commitment (par={par}) = {}",
            p.commitment
        );
        let o = p.delta.outcome;
        check!(
            n,
            (o.applied, o.noops, o.outputs, o.fees_to_beneficiary) == (2, 1, 4, 62_000_000_000_000),
            "V3 outcome (par={par}) = {o:?}"
        );
        check!(
            n,
            p.delta.evict == vec![t0_hash, keccak256(tx1.encode()), keccak256(tx2.encode())],
            "V3 items (par={par})"
        );
        let mut post = pre.clone();
        lean_core::apply(&mut post, &p.delta);
        check!(
            n,
            sorted(&post) == v3_post,
            "V3 post-state (par={par}) = {:?}",
            sorted(&post)
        );
        check!(
            n,
            !post.accounts.contains_key(&rep(0x33)),
            "V3: no-op recipient 0x33… must be absent"
        );
        check!(
            n,
            post.digest() == v3_digest,
            "V3 post-digest (par={par}) = {}",
            post.digest()
        );
    }

    // ---- V4 ----
    let empty_list: [Vec<u8>; 0] = [];
    check!(
        n,
        keccak256(framing::txs_section(&empty_list))
            == b256("0xe8e77626586f73b955364c7b4bbf0bb7f7685ebd40e852b164633a4acbd3244c"),
        "V4 txs_hash(no entries)"
    );
    let hdr2 = BlockHeader {
        parent: v3_commit,
        number: 2,
        timestamp_ms: 1_700_000_000_500,
    };
    let wire4 = framing::to_wire(&hdr2, &empty_list);
    check!(
        n,
        wire4 == h(V4_WIRE),
        "V4 wire = 0x{}",
        hex::encode(&wire4)
    );
    let v4_commit = b256("0x0d99c95bba358956556d8b60084682c479195007ad0ef8792acad9746e205a40");
    let empty_state = FlatState::default();
    for par in [true, false] {
        let p = lean_core::process_block(&empty_state, &wire4, &ctx(par))
            .map_err(|e| format!("V4 process_block: {e}"))?;
        check!(
            n,
            p.commitment == v4_commit,
            "V4 commitment (par={par}) = {}",
            p.commitment
        );
        let mut post = FlatState::default();
        lean_core::apply(&mut post, &p.delta);
        check!(
            n,
            post.digest()
                == b256("0x6e1bd6f9532cfbf5580724858fcedc7089a23d8307c0594ef5dd150f4c1fda9f"),
            "V4 empty block on empty state digest (par={par}) = {}",
            post.digest()
        );
    }

    // ---- V5 ----
    let mut twin = tx0.clone();
    twin.sig = high_s_twin(&tx0.sig);
    check!(n, twin.sig[64] == 1, "V5 v' = 1");
    let twin_b = twin.encode();
    check!(
        n,
        twin_b == h(V5_HEX),
        "V5 bytes = 0x{}",
        hex::encode(&twin_b)
    );
    check!(
        n,
        keccak256(&twin_b)
            == b256("0x5b3580c6a86476b5c39c7c368793b22bf7299b3ad51257c73e6be202fe8b340c"),
        "V5 tx_hash"
    );
    check!(
        n,
        twin.recover_sender(&domain()) == Ok(addr(K1)),
        "V5 twin recovers to addr(k1)"
    );
    let wire5 = framing::to_wire(&hdr1, std::slice::from_ref(&twin_b));
    for par in [true, false] {
        let p = lean_core::process_block(&pre, &wire5, &ctx(par))
            .map_err(|e| format!("V5 process_block: {e}"))?;
        check!(
            n,
            p.delta.outcome.applied == 1 && p.delta.outcome.noops == 0,
            "V5 twin accepted in a block (par={par}): {:?}",
            p.delta.outcome
        );
    }

    // ---- V6 ----
    for v in [2u8, 3, 4, 27, 28] {
        let mut b = t0b.clone();
        b[127] = v;
        for par in [true, false] {
            let items = lean_core::recover_txs(&[b.clone()], &ctx(par));
            check!(n, items.is_empty(), "V6 v={v} must be skipped (par={par})");
            let p =
                lean_core::process_block(&pre, &framing::to_wire(&hdr1, &[b.clone()]), &ctx(par))
                    .map_err(|e| format!("V6 process_block: {e}"))?;
            let o = p.delta.outcome;
            check!(
                n,
                o.applied == 0 && o.noops == 0 && p.delta.evict.is_empty(),
                "V6 v={v}: skipped entries are not counted (par={par}): {o:?}"
            );
        }
    }

    // ---- V7 ----
    let mut t0_dead = t0b.clone();
    t0_dead.extend([0xde, 0xad]);
    check!(
        n,
        LeanTx::decode(&t0_dead) == Err(lean_native::DecodeError::TrailingBytes),
        "V7 strict decode must fail with TrailingBytes: {:?}",
        LeanTx::decode(&t0_dead)
    );
    for par in [true, false] {
        let d = lean_core::decode_txs(&[t0_dead.clone()], &ctx(par));
        check!(
            n,
            d.len() == 1 && d[0].hash == t0_hash && d[0].tx == tx0,
            "V7 entry decodes to T0 with T0's hash (par={par})"
        );
        let wire7 = framing::to_wire(&hdr1, &[t0_dead.clone()]);
        let p = lean_core::process_block(&pre, &wire7, &ctx(par))
            .map_err(|e| format!("V7 process_block: {e}"))?;
        let mut sect = 1u32.to_le_bytes().to_vec();
        sect.extend(130u32.to_le_bytes());
        sect.extend(&t0_dead);
        check!(
            n,
            p.commitment == framing::commitment(&hdr1, keccak256(&sect)),
            "V7 commitment covers all 130 bytes (par={par})"
        );
        check!(
            n,
            p.delta.evict == vec![t0_hash] && p.delta.outcome.applied == 1,
            "V7 executes as T0 (par={par})"
        );
    }

    // ---- V8 ----
    let t9 = small_key(9).sign(0, vec![out(rep(0x44), 1)]);
    let entries8 = vec![
        vec![],
        vec![0x50, 0x00, 0x00],
        vec![0x02, 0x01],
        t9.encode(),
    ];
    let hdr8 = BlockHeader {
        parent: lean_core::genesis_commitment(1338),
        number: 1,
        timestamp_ms: 0,
    };
    let wire8 = framing::to_wire(&hdr8, &entries8);
    for par in [true, false] {
        let p = lean_core::process_block(&empty_state, &wire8, &ctx(par))
            .map_err(|e| format!("V8 process_block: {e}"))?;
        check!(
            n,
            p.commitment
                == b256("0xf89650c46f7da024f15e6c5530d66bd7e076b350674a808df40f9139ab58f18c"),
            "V8 commitment (par={par}) = {}",
            p.commitment
        );
        let o = p.delta.outcome;
        check!(
            n,
            p.delta.evict.len() == 1
                && (o.applied, o.noops, o.outputs, o.fees_to_beneficiary) == (0, 1, 0, 0),
            "V8 items/outcome (par={par}): {} items, {o:?}",
            p.delta.evict.len()
        );
        let mut post = FlatState::default();
        lean_core::apply(&mut post, &p.delta);
        let mut want = vec![(beneficiary(), 0, 0), (addr(K9), 0, 0)];
        want.sort();
        check!(
            n,
            sorted(&post) == want,
            "V8 post-state (par={par}) = {:?}",
            sorted(&post)
        );
        check!(
            n,
            post.digest()
                == b256("0x162235580f8292ac3bf1e0091420f8a16d448afa47cd4a6c687b8c7061285fff"),
            "V8 post-digest (par={par}) = {}",
            post.digest()
        );
    }
    Ok(*n)
}
