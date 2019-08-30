//
// Copyright 2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Owned coins
//!
//!

use bitcoin_hashes::sha256d;
use bitcoin::{OutPoint, TxOut, Script, Transaction};
use bitcoin::Block;
use std::collections::HashMap;
use account::{MasterAccount, KeyDerivation};
use proved::ProvedTransaction;
use rand::thread_rng;

#[derive(Clone, Debug, Eq, PartialEq)]
/// a coin is defined by the spendable output
/// the key derivation that allows to spend it
pub struct Coin {
    pub output: TxOut,
    pub derivation: KeyDerivation
}

/// Manage coins
#[derive(Eq, PartialEq)]
pub struct Coins {
    /// unconfirmed coins
    unconfirmed: HashMap<OutPoint, Coin>,
    /// confirmed coins (these have SPV proofs)
    confirmed: HashMap<OutPoint, Coin>,
    /// SPV proofs of transactions confirming coins
    proofs: HashMap<sha256d::Hash, ProvedTransaction>,
}

impl Coins {
    pub fn new () -> Coins {
        Coins { confirmed: HashMap::new(), proofs: HashMap::new(), unconfirmed: HashMap::new() }
    }

    /// this should only be used to restore previously computed state
    pub fn add_confirmed(&mut self, point: OutPoint, coin: Coin, proof: ProvedTransaction) {
        self.confirmed.insert(point, coin);
        self.proofs.insert(proof.get_transaction().txid(), proof);
    }

    pub fn remove_confirmed(&mut self, point: &OutPoint) -> bool {
        let modified = self.confirmed.remove(point).is_some();
        if modified && self.confirmed.iter().any(|(p, _)| p.txid == point.txid) == false {
            self.proofs.remove(&point.txid);
        }
        modified
    }

    /// process an unconfirmed transaction. Useful eg. to process own spends.
    pub fn process_unconfirmed_transaction(&mut self, master_account: &mut MasterAccount, transaction: &Transaction) -> bool {
        let mut scripts: HashMap<Script, KeyDerivation> = master_account.get_scripts().collect();
        let mut modified = false;
        for input in transaction.input.iter() {
            modified |= self.remove_confirmed(&input.previous_output);
        }
        for (vout, output) in transaction.output.iter().enumerate() {
            let mut lookahead = Vec::new();
            if let Some(d) = scripts.get(&output.script_pubkey) {
                lookahead =
                    master_account.get_mut((d.account, d.sub)).unwrap().do_look_ahead(Some(d.kix)).unwrap()
                        .iter().map(move |(kix, s)| (s.clone(), KeyDerivation { kix: *kix, account: d.account, sub: d.sub, tweak: d.tweak.clone(), csv: d.csv.clone() })).collect();
                self.unconfirmed.insert(OutPoint { txid: transaction.txid(), vout: vout as u32 },
                                      Coin { output: output.clone(), derivation: d.clone() });
                modified = true;
            }
            for (s, d) in lookahead {
                scripts.insert(s.clone(), d);
            }
        }
        modified
    }

    pub fn confirmed(&self) -> &HashMap<OutPoint, Coin> {
        &self.confirmed
    }

    pub fn unconfirmed(&self) -> &HashMap<OutPoint, Coin> {
        &self.unconfirmed
    }

    pub fn proofs(&self) -> &HashMap<sha256d::Hash, ProvedTransaction> {
        &self.proofs
    }

    pub fn confirmed_balance(&self) -> u64 {
        self.confirmed.values().map(|c| c.output.value).sum::<u64>()
    }

    pub fn unconfirmed_balance(&self) -> u64 {
        self.unconfirmed.values().map(|c| c.output.value).sum::<u64>()
    }

    /// unwind the tip of the trunk
    pub fn unwind_tip(&mut self, block_hash: &sha256d::Hash) {
        // this means we might have lost control of coins at least temporarily
        let lost_coins = self.proofs.values()
            .filter_map(|t| if *t.get_block_hash() == *block_hash {
                Some(t.get_transaction().txid())
            } else { None })
            .flat_map(|txid| self.confirmed.keys().filter(move |point| point.txid == txid)).cloned().collect::<Vec<OutPoint>>();

        for point in lost_coins {
            self.proofs.remove(&point.txid);
            let coin = self.confirmed.remove(&point).unwrap();
            self.unconfirmed.insert(point, coin);
        }
    }

    /// process a block to find own coins
    /// processing should be in ascending height order, it is fine to skip blocks  if you know
    /// there is nothing in them you would care (this will be easy to tell with committed BIP158
    /// filters, but we are not yet there)
    pub fn process(&mut self, master_account: &mut MasterAccount, block: &Block) -> bool {
        let mut scripts: HashMap<Script, KeyDerivation> = master_account.get_scripts().collect();

        let mut modified = false;
        for (txnr, tx) in block.txdata.iter().enumerate() {
            if txnr > 0 { // skip coinbase
                for input in tx.input.iter() {
                    modified |= self.remove_confirmed(&input.previous_output);
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let mut lookahead = Vec::new();
                if let Some(d) = scripts.get(&output.script_pubkey) {
                    lookahead =
                        master_account.get_mut((d.account, d.sub)).unwrap().do_look_ahead(Some(d.kix)).unwrap()
                            .iter().map(move |(kix, s)| (s.clone(), KeyDerivation{ kix: *kix, account: d.account, sub: d.sub, tweak: d.tweak.clone(), csv: d.csv.clone()})).collect();
                    let point = OutPoint { txid: tx.txid(), vout: vout as u32 };
                    self.unconfirmed.remove(&point);
                    self.confirmed.insert(point, Coin { output: output.clone(), derivation: d.clone()});
                    self.proofs.entry(tx.txid()).or_insert(ProvedTransaction::new(block, txnr));
                    modified = true;
                }
                for (s, d) in lookahead {
                    scripts.insert(s.clone(), d);
                }
            }
        }
        modified
    }

    /// get random confirmed coins of sufficient amount
    /// returns a vector of spent outpoins, coins and their confirmation height
    pub fn get_confirmed_coins<H> (&self,  minimum: u64, height: u32, block_height: H) -> Vec<(OutPoint, Coin, u32)>
        where H: Fn(&sha256d::Hash) -> Option<u32> {
        use rand::prelude::SliceRandom;
        // TODO: knapsack
        let mut sum = 0u64;
        let mut have = self.confirmed.iter()
            .filter_map( |(p,c)| {
                let proof = self.proofs.get(&p.txid).expect("missing proof of confirmed transaction");
                let h = block_height(proof.get_block_hash()).expect("coin not confirmed");
                if let Some(csv) = c.derivation.csv {
                    if h + (csv as u32) < height {
                        return None;
                    }
                }
                return Some(((*p).clone(), (*c).clone(), h));
            }).collect::<Vec<_>>();
        have.sort_by(|(_, a,_), (_, b,_)| a.output.value.cmp(&b.output.value));
        let mut inputs = Vec::new();
        for (point, coin,height) in have.iter() {
            sum += coin.output.value;
            inputs.push(((*point).clone(), (*coin).clone(), *height));
            if sum >= minimum {
                break;
            }
        }
        if sum > minimum {
            let mut change = sum - minimum;
            // drop some if possible
            while let Some(index) = inputs.iter().enumerate().find_map(|(i, (_, c,_))| if c.output.value <= change { Some(i) } else { None }) {
                let removed = inputs[index].1.output.value;
                change -= removed;
                inputs.remove(index);
            }
        }
        inputs.shuffle(&mut thread_rng());
        inputs
    }

}
