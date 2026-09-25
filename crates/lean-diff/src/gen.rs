//! Seeded generator of adversarial lean chains from a funded genesis.
//!
//! A chain is: a fund file (varied spellings) + fund balance, then blocks.
//! Each block is drawn against the CURRENT committed state (the oracle's,
//! passed in) so that "exact nonce", "exact balance" and "spend a credit
//! received this block" can be aimed precisely; an in-block model tracks the
//! nonces/debits/credits the block itself causes. The model only STEERS the
//! generator — it checks nothing; engine and oracle are compared on whatever
//! comes out.
//!
//! Entry kinds and their default weights are in [`Rates`]; any subset can be
//! overridden with a spec string such as `valid=10,bad_sig=0,empty_block=0.2`
//! (env `LEAN_DIFF_RATES`, or `--rates` on the binary).
//!
//! Balances: see the crate docs — `fund_balance * funded` stays below 2^110.

use crate::rng::Rng;
use crate::util::{add_be, be32_from_u64, beneficiary, high_s_twin, keys, Key, N};
use alloy_primitives::Address;
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT, MAX_OUTPUTS, SIG_LEN};
use lean_oracle::{encode_block, Header, State};
use std::collections::HashMap;

/// Entry kinds, in [`Rates::weights`] order.
pub const KINDS: &[&str] = &[
    "valid",
    "exact_balance",
    "over_by_one",
    "insufficient",
    "stale_nonce",
    "future_nonce",
    "replay",
    "dup_in_block",
    "credit_spend",
    "burst",
    "high_s",
    "v_variant",
    "bad_sig",
    "malformed",
    "trailing",
    "unfunded",
    "garbage",
];

const K_VALID: usize = 0;
const K_EXACT: usize = 1;
const K_OVER: usize = 2;
const K_INSUFF: usize = 3;
const K_STALE: usize = 4;
const K_FUTURE: usize = 5;
const K_REPLAY: usize = 6;
const K_DUP: usize = 7;
const K_CREDIT: usize = 8;
const K_BURST: usize = 9;
const K_HIGHS: usize = 10;
const K_V: usize = 11;
const K_BADSIG: usize = 12;
const K_MALFORMED: usize = 13;
const K_TRAILING: usize = 14;
const K_UNFUNDED: usize = 15;
const K_GARBAGE: usize = 16;

/// Upper bound on the genesis total supply (see crate docs).
pub const SUPPLY_CAP_LOG2: u32 = 110;

/// Controllable adversarial rates.
#[derive(Clone, Debug)]
pub struct Rates {
    /// Relative weight of each entry kind (index = position in [`KINDS`]).
    pub weights: [u32; KINDS.len()],
    /// Probability that a block has no entries at all.
    pub empty_block: f64,
    /// Probability that a block's wire framing is corrupted.
    pub corrupt_framing: f64,
    /// Max entries drawn for one block (bursts may overshoot slightly).
    pub max_entries: usize,
    /// Probability that a drawn fan-out is large (301..=MAX_OUTPUTS, incl. MAX).
    pub big_fanout: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Rates {
            //        va ex ov in st fu re du cr bu hs  v bs ma tr un ga
            weights: [30, 5, 4, 4, 4, 4, 4, 3, 4, 5, 4, 4, 5, 6, 4, 3, 3],
            empty_block: 0.05,
            corrupt_framing: 0.04,
            max_entries: 40,
            big_fanout: 0.004,
        }
    }
}

impl Rates {
    /// Default rates with `k=v,k=v` overrides (kind names, or `empty_block`,
    /// `corrupt_framing`, `max_entries`, `big_fanout`).
    pub fn parse(spec: &str) -> Result<Rates, String> {
        let mut r = Rates::default();
        for kv in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("rates: '{kv}' is not k=v"))?;
            let bad = |e: &dyn std::fmt::Display| format!("rates: {k}={v}: {e}");
            match k {
                "empty_block" => r.empty_block = v.parse().map_err(|e| bad(&e))?,
                "corrupt_framing" => r.corrupt_framing = v.parse().map_err(|e| bad(&e))?,
                "max_entries" => r.max_entries = v.parse().map_err(|e| bad(&e))?,
                "big_fanout" => r.big_fanout = v.parse().map_err(|e| bad(&e))?,
                _ => {
                    let i = KINDS
                        .iter()
                        .position(|x| *x == k)
                        .ok_or_else(|| format!("rates: unknown key '{k}' (kinds: {KINDS:?})"))?;
                    r.weights[i] = v.parse().map_err(|e| bad(&e))?;
                }
            }
        }
        if r.weights.iter().all(|&w| w == 0) {
            return Err("rates: all kind weights are zero".into());
        }
        r.max_entries = r.max_entries.max(1);
        Ok(r)
    }

    /// `LEAN_DIFF_RATES` if set, else the defaults.
    pub fn from_env() -> Result<Rates, String> {
        match std::env::var("LEAN_DIFF_RATES") {
            Ok(s) => Rates::parse(&s),
            Err(_) => Ok(Rates::default()),
        }
    }
}

/// One generated block.
#[derive(Clone, Debug)]
pub struct GenBlock {
    pub header: Header,
    pub entries: Vec<Vec<u8>>,
    /// Kind label of each entry (same length as `entries`).
    pub kinds: Vec<&'static str>,
    /// The bytes handed to both implementations.
    pub wire: Vec<u8>,
    /// Framing corruption applied to `wire`, if any.
    pub corruption: Option<&'static str>,
}

/// In-block steering model (NOT a checker).
struct Model<'a> {
    base: &'a State,
    nonce: HashMap<[u8; 20], u64>,
    spend: HashMap<[u8; 20], u128>,
    credit: HashMap<[u8; 20], u128>,
}

impl<'a> Model<'a> {
    fn new(base: &'a State) -> Self {
        Model {
            base,
            nonce: HashMap::new(),
            spend: HashMap::new(),
            credit: HashMap::new(),
        }
    }
    fn nonce(&self, a: &[u8; 20]) -> u64 {
        *self.nonce.get(a).unwrap_or(&self.base.get(a).nonce)
    }
    fn spend(&self, a: &[u8; 20]) -> u128 {
        *self.spend.get(a).unwrap_or(&self.base.get(a).balance)
    }
    fn credit(&self, a: &[u8; 20]) -> u128 {
        *self.credit.get(a).unwrap_or(&0)
    }
    /// A tx whose sender we know is about to be included.
    fn note(&mut self, sender: [u8; 20], tx: &LeanTx) {
        let mut total = lean_fee(tx.outputs.len());
        for o in &tx.outputs {
            total = total.saturating_add(o.amount as u128 * AMOUNT_UNIT);
        }
        let (n, s) = (self.nonce(&sender), self.spend(&sender));
        if tx.nonce as u64 == n && s >= total {
            self.nonce.insert(sender, n + 1);
            self.spend.insert(sender, s - total);
            for o in &tx.outputs {
                *self.credit.entry(o.to.0 .0).or_default() += o.amount as u128 * AMOUNT_UNIT;
            }
        }
    }
}

/// The generator for one chain.
pub struct ChainGen {
    rng: Rng,
    rates: Rates,
    /// Participating keys (indices into the shared pool) and whether funded.
    keys: Vec<(&'static Key, bool)>,
    /// Pool keys NOT in this chain: always-unfunded senders.
    outsiders: Vec<&'static Key>,
    strangers: Vec<[u8; 20]>,
    history: Vec<Vec<u8>>,
    ts: u64,
    pub fund_file: String,
    pub fund_balance: u128,
}

fn a20(a: &Address) -> [u8; 20] {
    a.0 .0
}

impl ChainGen {
    pub fn new(seed: u64, rates: Rates) -> Self {
        let mut rng = Rng::new(seed);
        let pool = keys();
        let n_keys = rng.usize(2, 24);
        let mut idx: Vec<usize> = (0..pool.len()).collect();
        for i in 0..n_keys {
            let j = rng.usize(i, idx.len() - 1);
            idx.swap(i, j);
        }
        let mut chain_keys: Vec<(&'static Key, bool)> = idx[..n_keys]
            .iter()
            .map(|&i| (&pool[i], rng.chance(0.85)))
            .collect();
        chain_keys[0].1 = true;
        let outsiders = idx[n_keys..n_keys + 4].iter().map(|&i| &pool[i]).collect();
        let strangers: Vec<[u8; 20]> = (0..32)
            .map(|_| {
                let mut a = [0u8; 20];
                rng.fill(&mut a);
                a
            })
            .collect();
        let funded_strangers: Vec<[u8; 20]> = strangers
            .iter()
            .filter(|_| rng.chance(0.15))
            .copied()
            .collect();
        let funded_count = chain_keys.iter().filter(|k| k.1).count() + funded_strangers.len();

        // Fund balance: tiny / default / large / huge (capped), mostly a
        // multiple of 1 gwei so exact-balance txs are possible.
        let cap = (1u128 << SUPPLY_CAP_LOG2) / funded_count as u128;
        let mut bal: u128 = match rng.weighted(&[20, 20, 30, 20, 8, 2]) {
            0 => lean_fee(1) * rng.range(1, 6) as u128 + rng.below(50_000) as u128 * AMOUNT_UNIT,
            1 => 1_000_000_000_000_000,
            2 => 1_000_000_000_000_000_000 * rng.range(1, 1000) as u128,
            3 => rng.below_u128(cap),
            4 => cap - 1,
            _ => 0,
        };
        if rng.chance(0.9) {
            bal -= bal % AMOUNT_UNIT;
        }

        // Fund file in varied spellings (all of which SPEC §3.2 accepts).
        let mut lines: Vec<String> = Vec::new();
        let mut funded_addrs: Vec<[u8; 20]> = chain_keys
            .iter()
            .filter(|k| k.1)
            .map(|k| a20(&k.0.addr))
            .collect();
        funded_addrs.extend(&funded_strangers);
        for a in &funded_addrs {
            let copies = if rng.chance(0.1) { 2 } else { 1 };
            for _ in 0..copies {
                let h = hex::encode(a);
                let body = match rng.below(5) {
                    0 => format!("0x{h}"),
                    1 => h.clone(),
                    2 => format!("0x{}", h.to_uppercase()),
                    3 => Address::from(*a).to_checksum(None),
                    _ => {
                        let mixed: String = h
                            .chars()
                            .map(|c| {
                                if rng.chance(0.5) {
                                    c.to_ascii_uppercase()
                                } else {
                                    c
                                }
                            })
                            .collect();
                        format!("0x{mixed}")
                    }
                };
                let pad = |rng: &mut Rng| match rng.below(4) {
                    0 => " ",
                    1 => "\t",
                    2 => "  \t ",
                    _ => "",
                };
                lines.push(format!("{}{}{}", pad(&mut rng), body, pad(&mut rng)));
                if rng.chance(0.1) {
                    lines.push(String::new());
                }
            }
        }
        let sep = if rng.chance(0.2) { "\r\n" } else { "\n" };
        let fund_file = lines.join(sep);

        ChainGen {
            rng,
            rates,
            keys: chain_keys,
            outsiders,
            strangers,
            history: Vec::new(),
            ts: 1_700_000_000_000,
            fund_file,
            fund_balance: bal,
        }
    }

    /// Draw block `number` on `parent` against committed `state`.
    pub fn next_block(&mut self, state: &State, parent: [u8; 32], number: u64) -> GenBlock {
        self.ts = if self.rng.chance(0.03) {
            self.rng.next_u64()
        } else {
            self.ts.wrapping_add(self.rng.range(0, 1000))
        };
        let header = Header {
            parent,
            number,
            timestamp_ms: self.ts,
        };
        let mut b = Building {
            entries: Vec::new(),
            kinds: Vec::new(),
        };
        if !self.rng.chance(self.rates.empty_block) {
            let target = if self.rng.chance(0.5) {
                self.rng.usize(1, 8.min(self.rates.max_entries))
            } else {
                self.rng.usize(1, self.rates.max_entries)
            };
            let mut model = Model::new(state);
            while b.entries.len() < target {
                let kind = self.rng.weighted(&self.rates.weights);
                self.emit(kind, &mut model, &mut b);
            }
        }
        let mut wire = encode_block(&header, &b.entries);
        let corruption = if self.rng.chance(self.rates.corrupt_framing) {
            Some(self.corrupt(&mut wire, &b.entries))
        } else {
            None
        };
        for e in &b.entries {
            if self.history.len() < 512 {
                self.history.push(e.clone());
            } else {
                let i = self.rng.below(512) as usize;
                self.history[i] = e.clone();
            }
        }
        GenBlock {
            header,
            entries: b.entries,
            kinds: b.kinds,
            wire,
            corruption,
        }
    }

    // ---- building blocks --------------------------------------------------

    fn funded_key(&mut self) -> &'static Key {
        let funded: Vec<&'static Key> = self.keys.iter().filter(|k| k.1).map(|k| k.0).collect();
        if self.rng.chance(0.9) {
            funded[self.rng.below(funded.len() as u64) as usize]
        } else {
            self.rng.pick(&self.keys).0
        }
    }

    fn pick_n(&mut self) -> usize {
        if self.rng.chance(self.rates.big_fanout) {
            return if self.rng.chance(0.3) {
                MAX_OUTPUTS
            } else {
                self.rng.usize(301, MAX_OUTPUTS)
            };
        }
        match self.rng.below(1000) {
            0..600 => self.rng.usize(1, 3),
            600..850 => self.rng.usize(4, 16),
            850..970 => self.rng.usize(17, 100),
            _ => self.rng.usize(101, 300),
        }
    }

    fn recipient(&mut self, sender: &Address, prev: &[Output]) -> Address {
        match self.rng.below(100) {
            0..35 => self.rng.pick(&self.keys).0.addr,
            35..60 => Address::from(*self.rng.pick(&self.strangers)),
            60..70 => {
                let mut a = [0u8; 20];
                self.rng.fill(&mut a);
                Address::from(a)
            }
            70..78 => *sender,
            78..83 => Address::ZERO,
            83..88 => beneficiary(),
            _ => match prev.last() {
                Some(o) if self.rng.chance(0.5) => o.to,
                Some(_) => prev[self.rng.below(prev.len() as u64) as usize].to,
                None => self.rng.pick(&self.keys).0.addr,
            },
        }
    }

    /// `n` outputs whose amounts sum to at most `budget` gwei units.
    fn outputs(&mut self, sender: &Address, n: usize, budget: u128) -> Vec<Output> {
        let per = budget / n as u128;
        let mut left = budget;
        let mut outs: Vec<Output> = Vec::with_capacity(n);
        for _ in 0..n {
            let amount = if self.rng.chance(0.08) {
                0
            } else {
                let want = self.rng.below_u128(per.saturating_mul(2) + 1).min(left);
                want.min(u64::MAX as u128) as u64
            };
            left -= amount as u128;
            let to = self.recipient(sender, &outs);
            outs.push(Output { to, amount });
        }
        outs
    }

    /// Spendable gwei units for `n` outputs after the fee (0 if unaffordable).
    fn avail_units(m: &Model, a: &[u8; 20], n: usize) -> u128 {
        m.spend(a).saturating_sub(lean_fee(n)) / AMOUNT_UNIT
    }

    fn budget(&mut self, avail: u128) -> u128 {
        match self.rng.below(5) {
            0 => avail.min(1_000),
            1 => avail / 1000,
            2 => avail / 10,
            3 => avail / 2,
            _ => self.rng.below_u128(avail + 1),
        }
    }

    /// A tx from `k` that the model expects to apply (exact nonce, affordable).
    fn valid_tx(&mut self, k: &'static Key, m: &Model) -> LeanTx {
        let n = self.pick_n();
        let avail = Self::avail_units(m, &a20(&k.addr), n);
        let budget = self.budget(avail);
        let outs = self.outputs(&k.addr, n, budget);
        k.sign(m.nonce(&a20(&k.addr)) as u32, outs)
    }

    fn push_tx(&mut self, b: &mut Building, m: &mut Model, k: &Key, tx: &LeanTx, kind: usize) {
        m.note(a20(&k.addr), tx);
        b.push(tx.encode(), kind);
    }

    /// Exact-balance tx (total == spendable), or None if the balance does not
    /// allow one.
    fn exact_tx(&mut self, k: &'static Key, m: &Model, plus_one: bool) -> Option<LeanTx> {
        let a = a20(&k.addr);
        let spend = m.spend(&a);
        if !spend.is_multiple_of(AMOUNT_UNIT) || spend < lean_fee(1) {
            return None;
        }
        let mut n = self.rng.usize(1, 4);
        loop {
            if spend < lean_fee(n) {
                n = 1;
            }
            let units = (spend - lean_fee(n)) / AMOUNT_UNIT;
            let need = units.div_ceil(u64::MAX as u128 - 64).max(1) as usize;
            if need > n {
                if need > MAX_OUTPUTS {
                    return None;
                }
                n = need;
                continue;
            }
            let mut amounts = vec![(units / n as u128) as u64; n];
            amounts[0] += (units % n as u128) as u64;
            for _ in 0..n.min(8) {
                let (i, j) = (self.rng.usize(0, n - 1), self.rng.usize(0, n - 1));
                let room = u64::MAX - amounts[j];
                let mv = self.rng.below(amounts[i].min(room).saturating_add(1));
                amounts[i] -= mv;
                amounts[j] += mv;
            }
            if plus_one {
                let i = amounts.iter().position(|&x| x < u64::MAX)?;
                amounts[i] += 1;
            }
            let mut outs: Vec<Output> = Vec::with_capacity(n);
            for amount in amounts {
                let to = self.recipient(&k.addr, &outs);
                outs.push(Output { to, amount });
            }
            return Some(k.sign(m.nonce(&a) as u32, outs));
        }
    }

    fn emit(&mut self, kind: usize, m: &mut Model, b: &mut Building) {
        match kind {
            K_VALID => {
                let k = self.funded_key();
                let tx = self.valid_tx(k, m);
                self.push_tx(b, m, k, &tx, K_VALID);
            }
            K_EXACT | K_OVER => {
                let k = self.funded_key();
                match self.exact_tx(k, m, kind == K_OVER) {
                    Some(tx) => self.push_tx(b, m, k, &tx, kind),
                    None => self.emit(K_VALID, m, b),
                }
            }
            K_INSUFF => {
                let k = self.funded_key();
                let n = self.rng.usize(1, 4);
                let mut outs: Vec<Output> = Vec::with_capacity(n);
                for _ in 0..n {
                    let amount = if self.rng.chance(0.3) {
                        u64::MAX
                    } else {
                        self.rng.next_u64()
                    };
                    let to = self.recipient(&k.addr, &outs);
                    outs.push(Output { to, amount });
                }
                let tx = k.sign(m.nonce(&a20(&k.addr)) as u32, outs);
                self.push_tx(b, m, k, &tx, K_INSUFF);
            }
            K_STALE | K_FUTURE => {
                let k = self.funded_key();
                let cur = m.nonce(&a20(&k.addr));
                let nonce: u64 = if kind == K_STALE && cur > 0 {
                    cur - 1 - self.rng.below(cur.min(4))
                } else {
                    match self.rng.below(10) {
                        0 => u32::MAX as u64,
                        1 => cur + (1 << 31),
                        _ => cur + 1 + self.rng.below(4),
                    }
                };
                let n = self.rng.usize(1, 3);
                let avail = Self::avail_units(m, &a20(&k.addr), n);
                let outs = self.outputs(&k.addr, n, avail / 100);
                let tx = k.sign(nonce as u32, outs);
                self.push_tx(b, m, k, &tx, kind);
            }
            K_REPLAY => {
                if self.history.is_empty() {
                    return self.emit(K_VALID, m, b);
                }
                let e = self.rng.pick(&self.history).clone();
                b.push(e, K_REPLAY);
            }
            K_DUP => {
                if b.entries.is_empty() {
                    return self.emit(K_VALID, m, b);
                }
                let e = self.rng.pick(&b.entries).clone();
                b.push(e, K_DUP);
            }
            K_CREDIT => self.credit_spend(m, b),
            K_BURST => {
                let k = self.funded_key();
                let a = a20(&k.addr);
                let count = if self.rng.chance(0.2) {
                    self.rng.usize(9, 30)
                } else {
                    self.rng.usize(2, 8)
                };
                let mut nonce = m.nonce(&a);
                for _ in 0..count {
                    if self.rng.chance(0.05) {
                        nonce += 1; // gap: the rest of the burst are future no-ops
                    } else if self.rng.chance(0.05) && nonce > 0 {
                        nonce -= 1; // repeat
                    }
                    let n = self.rng.usize(1, 3);
                    let avail = Self::avail_units(m, &a, n);
                    let outs = self.outputs(&k.addr, n, avail / (4 * count as u128));
                    let tx = k.sign(nonce as u32, outs);
                    self.push_tx(b, m, k, &tx, K_BURST);
                    nonce += 1;
                }
            }
            K_HIGHS => {
                let k = self.funded_key();
                let tx = self.valid_tx(k, m);
                let mut twin = tx.clone();
                twin.sig = high_s_twin(&tx.sig);
                match self.rng.below(3) {
                    0 => self.push_tx(b, m, k, &twin, K_HIGHS),
                    1 => {
                        self.push_tx(b, m, k, &twin, K_HIGHS);
                        self.push_tx(b, m, k, &tx, K_HIGHS);
                    }
                    _ => {
                        self.push_tx(b, m, k, &tx, K_HIGHS);
                        self.push_tx(b, m, k, &twin, K_HIGHS);
                    }
                }
            }
            K_V => {
                let k = self.funded_key();
                let mut tx = self.valid_tx(k, m);
                let v = *self.rng.pick(&[0u8, 1, 2, 3, 27, 28]);
                if v == tx.sig[64] {
                    self.push_tx(b, m, k, &tx, K_V);
                } else {
                    tx.sig[64] = v;
                    b.push(tx.encode(), K_V);
                }
            }
            K_BADSIG => {
                let k = self.funded_key();
                let tx = self.valid_tx(k, m);
                let bytes = self.bad_sig(tx);
                b.push(bytes, K_BADSIG);
            }
            K_MALFORMED => {
                let k = self.funded_key();
                let tx = self.valid_tx(k, m);
                let bytes = self.malformed(tx.encode(), tx.outputs.len());
                b.push(bytes, K_MALFORMED);
            }
            K_TRAILING => {
                let k = self.funded_key();
                let tx = self.valid_tx(k, m);
                let mut bytes = tx.encode();
                match self.rng.below(4) {
                    0 => bytes.extend(
                        k.sign(
                            0,
                            vec![Output {
                                to: Address::ZERO,
                                amount: 1,
                            }],
                        )
                        .encode(),
                    ),
                    1 => bytes.push(0x50),
                    _ => {
                        let extra = self.rng.usize(1, 64);
                        bytes.extend(self.rng.bytes(extra));
                    }
                }
                m.note(a20(&k.addr), &tx);
                b.push(bytes, K_TRAILING);
            }
            K_UNFUNDED => {
                let k = if self.rng.chance(0.5) {
                    *self.rng.pick(&self.outsiders)
                } else {
                    match self.keys.iter().find(|k| !k.1) {
                        Some(k) => k.0,
                        None => *self.rng.pick(&self.outsiders),
                    }
                };
                let n = self.rng.usize(1, 3);
                let outs = self.outputs(&k.addr, n, 10);
                let tx = k.sign(m.nonce(&a20(&k.addr)) as u32, outs);
                self.push_tx(b, m, k, &tx, K_UNFUNDED);
            }
            K_GARBAGE => {
                let len = self.rng.usize(0, 300);
                let mut g = self.rng.bytes(len);
                if !g.is_empty() && self.rng.chance(0.3) {
                    g[0] = 0x50;
                }
                b.push(g, K_GARBAGE);
            }
            _ => unreachable!("kind index"),
        }
    }

    /// A credited key tries to spend its same-block credit (must no-op).
    fn credit_spend(&mut self, m: &mut Model, b: &mut Building) {
        let keys: Vec<&'static Key> = self.keys.iter().map(|k| k.0).collect();
        let credited: Vec<&'static Key> = keys
            .iter()
            .copied()
            .filter(|k| m.credit(&a20(&k.addr)) > 0)
            .collect();
        let recv: &'static Key = if credited.is_empty() || self.rng.chance(0.3) {
            // First give someone a large credit.
            let giver = self.funded_key();
            let others: Vec<&'static Key> = keys
                .iter()
                .copied()
                .filter(|k| k.addr != giver.addr)
                .collect();
            if others.is_empty() {
                return self.emit(K_VALID, m, b);
            }
            let recv = *self.rng.pick(&others);
            let avail = Self::avail_units(m, &a20(&giver.addr), 1);
            let amount = (avail / 2).min(u64::MAX as u128) as u64;
            let tx = giver.sign(
                m.nonce(&a20(&giver.addr)) as u32,
                vec![Output {
                    to: recv.addr,
                    amount,
                }],
            );
            self.push_tx(b, m, giver, &tx, K_CREDIT);
            recv
        } else {
            credited[self.rng.below(credited.len() as u64) as usize]
        };
        let a = a20(&recv.addr);
        let (spend, credit, fee) = (m.spend(&a), m.credit(&a), lean_fee(1));
        let u_min = if spend >= fee {
            (spend - fee) / AMOUNT_UNIT + 1
        } else {
            0
        };
        let u_max = (spend + credit).saturating_sub(fee) / AMOUNT_UNIT;
        let u = if u_min <= u_max {
            u_min + self.rng.below_u128(u_max - u_min + 1)
        } else {
            u_min
        };
        let tx = recv.sign(
            m.nonce(&a) as u32,
            vec![Output {
                to: self.rng.pick(&keys).addr,
                amount: u.min(u64::MAX as u128) as u64,
            }],
        );
        self.push_tx(b, m, recv, &tx, K_CREDIT);
    }

    fn bad_sig(&mut self, tx: LeanTx) -> Vec<u8> {
        let mut t = tx;
        let set = |sig: &mut [u8; SIG_LEN], at: usize, v: [u8; 32]| {
            sig[at..at + 32].copy_from_slice(&v);
        };
        match self.rng.below(10) {
            0 => {
                let r = self.rng.bytes(64);
                t.sig[..64].copy_from_slice(&r);
                t.sig[64] = self.rng.below(2) as u8;
            }
            1 => set(&mut t.sig, 0, [0u8; 32]),
            2 => set(&mut t.sig, 32, [0u8; 32]),
            3 => set(&mut t.sig, 0, N),
            4 => set(&mut t.sig, 32, N),
            5 => set(
                &mut t.sig,
                0,
                add_be(&N, &be32_from_u64(self.rng.range(1, 1 << 40))),
            ),
            6 => set(&mut t.sig, 0, [0xff; 32]),
            7 => set(&mut t.sig, 32, crate::util::sub_be(&N, &be32_from_u64(1))),
            8 => {
                // Flip a body bit after signing: recovers to a random address.
                let mut bytes = t.encode();
                let body = 7 + 28 * t.outputs.len();
                let at = self.rng.usize(1, body - 1);
                bytes[at] ^= 1 << self.rng.below(8);
                return bytes;
            }
            _ => {
                // v in {2,3} with r < 2^128 so x = r + n < p: recovery CAN
                // succeed (SPEC §1.5, SPEC-QUESTIONS Q12) to a random key.
                let mut r = [0u8; 32];
                self.rng.fill(&mut r[16..]);
                r[31] |= 1;
                let mut s = [0u8; 32];
                self.rng.fill(&mut s);
                s[0] &= 0x7f;
                s[31] |= 1;
                set(&mut t.sig, 0, r);
                set(&mut t.sig, 32, s);
                t.sig[64] = 2 + self.rng.below(2) as u8;
            }
        }
        t.encode()
    }

    fn malformed(&mut self, mut t: Vec<u8>, n: usize) -> Vec<u8> {
        let set_n = |t: &mut Vec<u8>, v: u16| t[5..7].copy_from_slice(&v.to_le_bytes());
        match self.rng.below(12) {
            0 => {
                let len = t.len();
                let any = self.rng.usize(0, len - 1);
                let cut = *self.rng.pick(&[0, 1, 5, 7, 71, 72, len - 1, any]);
                t.truncate(cut.min(len - 1));
            }
            1 => {
                t[0] = *self
                    .rng
                    .pick(&[0x00, 0x01, 0x02, 0x03, 0x4f, 0x51, 0x7f, 0xc0, 0xf8, 0xff])
            }
            2 => set_n(&mut t, 0),
            3 => {
                let any = self.rng.range(10_001, 0xffff) as u16;
                let v = *self.rng.pick(&[10_001u16, 0xffff, any]);
                set_n(&mut t, v);
            }
            // claims more outputs than present (or > MAX when n is already MAX)
            4 => set_n(&mut t, (n + self.rng.usize(1, 3)) as u16),
            5 if n > 1 => set_n(&mut t, (n - 1) as u16),
            5 | 6 => t.clear(),
            7 => t = vec![0x50],
            8 => {
                t = vec![0x50];
                t.extend(vec![0u8; 71]);
                if self.rng.chance(0.5) {
                    // N = 1 but one byte short of 100.
                    t[5] = 1;
                    t.resize(99, 0xab);
                }
            }
            9 => t.truncate(7),
            10 => {
                let len = self.rng.usize(100, 200);
                t = vec![0x02];
                t.extend(self.rng.bytes(len));
            }
            _ => {
                let len = self.rng.usize(60, 120);
                t = vec![0xf8, len as u8];
                t.extend(self.rng.bytes(len));
            }
        }
        t
    }

    fn corrupt(&mut self, wire: &mut Vec<u8>, entries: &[Vec<u8>]) -> &'static str {
        // Offsets of each entry's u32 length prefix.
        let mut offs = Vec::with_capacity(entries.len());
        let mut at = 52;
        for e in entries {
            offs.push(at);
            at += 4 + e.len();
        }
        let n_txs = u32::from_le_bytes(wire[48..52].try_into().unwrap());
        let mut kind = self.rng.below(6);
        if offs.is_empty() && kind >= 4 {
            kind = 0;
        }
        match kind {
            0 => {
                let cut = self.rng.usize(0, wire.len() - 1);
                wire.truncate(cut);
                "truncated"
            }
            1 => {
                let extra = self.rng.usize(1, 8);
                wire.extend(self.rng.bytes(extra));
                "trailing bytes"
            }
            2 => {
                let v = n_txs.wrapping_add(self.rng.range(1, 3) as u32);
                wire[48..52].copy_from_slice(&v.to_le_bytes());
                "n_txs too large"
            }
            3 => {
                let v = if n_txs > 0 { n_txs - 1 } else { u32::MAX };
                wire[48..52].copy_from_slice(&v.to_le_bytes());
                "n_txs too small"
            }
            4 => {
                let o = *self.rng.pick(&offs);
                let len = u32::from_le_bytes(wire[o..o + 4].try_into().unwrap());
                let v = len.wrapping_add(self.rng.range(1, 4) as u32);
                wire[o..o + 4].copy_from_slice(&v.to_le_bytes());
                "entry length too large"
            }
            _ => {
                let o = *self.rng.pick(&offs);
                let len = u32::from_le_bytes(wire[o..o + 4].try_into().unwrap());
                let v = if len > 0 && self.rng.chance(0.5) {
                    len - 1
                } else {
                    u32::MAX
                };
                wire[o..o + 4].copy_from_slice(&v.to_le_bytes());
                "entry length corrupted"
            }
        }
    }
}

struct Building {
    entries: Vec<Vec<u8>>,
    kinds: Vec<&'static str>,
}

impl Building {
    fn push(&mut self, e: Vec<u8>, kind: usize) {
        self.entries.push(e);
        self.kinds.push(KINDS[kind]);
    }
}
