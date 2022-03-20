#![allow(unused_imports)]
#![allow(dead_code)]
use crate::header::BlockHeader;
use crate::invalid::{InvalidBlockError, InvalidBlockErrorReason};
use accountable::accountable::Accountable;
use claim::claim::Claim;
use log::info;
use rand::Rng;
use reward::reward::{Category, RewardState, GENESIS_REWARD};
use ritelinked::LinkedHashMap;
use serde::{Deserialize, Serialize};
use sha256::digest_bytes;
use state::state::NetworkState;
use std::fmt;
use txn::txn::Txn;
use verifiable::verifiable::Verifiable;

pub const NANO: u128 = 1;
pub const MICRO: u128 = NANO * 1000;
pub const MILLI: u128 = MICRO * 1000;
pub const SECOND: u128 = MILLI * 1000;

const VALIDATOR_THRESHOLD: f64 = 0.60;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[repr(C)]

    pub struct Block {
        pub header: BlockHeader,
        pub neighbors: Option<Vec<BlockHeader>>,
        pub height: u128,
        pub txns: LinkedHashMap<String, Txn>,
        pub claims: LinkedHashMap<String, Claim>,
        pub hash: String,
        pub received_at: Option<u128>,
        pub received_from: Option<String>,
        pub abandoned_claim: Option<Claim>,
    }

impl Block {
    // Returns a result with either a tuple containing the genesis block and the
    // updated account state (if successful) or an error (if unsuccessful)
    pub fn genesis(reward_state: &RewardState, claim: Claim, secret_key: String) -> Option<Block> {
        let header = BlockHeader::genesis(0, reward_state, claim.clone(), secret_key);
        let state_hash = digest_bytes(
            format!(
                "{},{}",
                header.last_hash,
                digest_bytes("Genesis_State_Hash".as_bytes())
            )
            .as_bytes(),
        );

        let mut claims = LinkedHashMap::new();
        claims.insert(claim.clone().pubkey.clone(), claim);

        let genesis = Block {
            header,
            neighbors: None,
            height: 0,
            txns: LinkedHashMap::new(),
            claims,
            hash: state_hash,
            received_at: None,
            received_from: None,
            abandoned_claim: None,
        };

        // Update the account state with the miner and new block, this will also set the values to the
        // network state. Unwrap the result and assign it to the variable updated_account_state to
        // be returned by this method.

        Some(genesis)
    }

    /// The mine method is used to generate a new block (and an updated account state with the reward set
    /// to the miner wallet's balance), this will also update the network state with a new confirmed state.
    pub fn mine(
        claim: Claim,      // The claim entitling the miner to mine the block.
        last_block: Block, // The last block, which contains the current block reward.
        txns: LinkedHashMap<String, Txn>,
        claims: LinkedHashMap<String, Claim>,
        claim_map_hash: Option<String>,
        reward_state: &RewardState,
        network_state: &NetworkState,
        neighbors: Option<Vec<BlockHeader>>,
        abandoned_claim: Option<Claim>,
        signature: String,
    ) -> Option<Block> {
        let txn_hash = {
            let mut txn_vec = vec![];
            txns.iter().for_each(|(_, v)| {
                txn_vec.extend(v.as_bytes());
            });
            digest_bytes(&txn_vec)
        };

        let neighbors_hash = {
            let mut neighbors_vec = vec![];
            if let Some(neighbors) = &neighbors {
                neighbors.iter().for_each(|v| {
                    neighbors_vec.extend(v.as_bytes());
                });
                Some(digest_bytes(&neighbors_vec))
            } else {
                None
            }
        };

        let header = BlockHeader::new(
            last_block.clone(),
            reward_state,
            claim,
            txn_hash,
            claim_map_hash,
            neighbors_hash,
            signature,
        );

        if let Some(time) = header.timestamp.checked_sub(last_block.header.timestamp) {
            if (time / SECOND) < 1 {
                return None;
            }
        } else {
            return None;
        }

        let height = last_block.height.clone() + 1;

        let mut block = Block {
            header: header.clone(),
            neighbors,
            height,
            txns,
            claims,
            hash: header.last_hash.clone(),
            received_at: None,
            received_from: None,
            abandoned_claim,
        };

        let mut hashable_state = network_state.clone();

        let hash = hashable_state.hash(&block.txns.clone(), block.header.block_reward.clone());
        block.hash = hash;
        Some(block)
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        self.to_string().as_bytes().to_vec()
    }

    pub fn from_bytes(data: &[u8]) -> Block {
        let mut buffer: Vec<u8> = vec![];

        data.iter().for_each(|x| buffer.push(*x));

        let to_string = String::from_utf8(buffer).unwrap();

        serde_json::from_str::<Block>(&to_string).unwrap()
    }

    pub fn to_string(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Block(\n \
            header: {:?},\n",
            self.header
        )
    }
}

impl Verifiable for Block {
    type Item = Block;
    type DependantOne = NetworkState;
    type DependantTwo = RewardState;
    type Error = InvalidBlockError;

    fn verifiable(&self) -> bool {
        true
    }

    #[allow(unused_variables)]
    fn valid(
        &self,
        item: &Self::Item,
        dependant_one: &Self::DependantOne,
        dependant_two: &Self::DependantTwo,
    ) -> Result<bool, Self::Error> {
        if self.header.block_height > item.header.block_height + 1 {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::BlockOutOfSequence,
            });
        }

        if self.header.block_height < item.header.block_height || self.header.block_height == item.header.block_height {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::NotTallestChain,
            });
        }

        if self.header.block_nonce != item.header.next_block_nonce {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockNonce,
            });
        }

        if self.header.block_reward.category != item.header.next_block_reward.category {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockReward,
            });
        }

        if self.header.block_reward.get_amount() != item.header.next_block_reward.get_amount() {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockReward,
            });
        }

        if let Some((hash, pointers)) =
            dependant_one.get_lowest_pointer(self.header.block_nonce as u128)
        {
            if hash == self.header.claim.hash {
                if let Some(claim_pointer) = self
                    .header
                    .claim
                    .get_pointer(self.header.block_nonce as u128)
                {
                    if pointers != claim_pointer {
                        return Err(Self::Error {
                            details: InvalidBlockErrorReason::InvalidClaimPointers,
                        });
                    }
                } else {
                    return Err(Self::Error {
                        details: InvalidBlockErrorReason::InvalidClaimPointers,
                    });
                }
            } else {
                return Err(Self::Error {
                    details: InvalidBlockErrorReason::InvalidClaimPointers,
                });
            }
        }

        if !dependant_two.valid_reward(self.header.block_reward.category) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockReward,
            });
        }

        if !dependant_two.valid_reward(self.header.next_block_reward.category) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidNextBlockReward,
            });
        }

        if self.header.last_hash != item.hash {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidLastHash,
            });
        }

        if let Err(_) = self.header.claim.valid(&None, &None, &None) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidClaim,
            });
        }

        Ok(true)
    }

    fn valid_genesis(&self, dependant_two: &Self::DependantTwo) -> Result<bool, Self::Error> {
        let genesis_last_hash = digest_bytes("Genesis_Last_Hash".as_bytes());
        let genesis_state_hash = digest_bytes(
            format!(
                "{},{}",
                genesis_last_hash,
                digest_bytes("Genesis_State_Hash".as_bytes())
            )
            .as_bytes(),
        );

        if self.header.block_height != 0 {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockHeight,
            });
        }

        if !dependant_two.valid_reward(self.header.block_reward.category) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockReward,
            });
        }

        if !dependant_two.valid_reward(self.header.next_block_reward.category) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidNextBlockReward,
            });
        }

        if self.header.last_hash != genesis_last_hash {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidLastHash,
            });
        }

        if self.hash != genesis_state_hash {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidStateHash,
            });
        }

        if let Err(_) = self.header.claim.valid(&None, &None, &None) {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidClaim,
            });
        }

        if let Err(_) = self.header.verify() {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidBlockSignature,
            });
        }

        let mut valid_data = true;
        self.txns.iter().for_each(|(_, txn)| {
            let n_valid = txn.validators.iter().filter(|(_, &valid)| valid).count();
            if (n_valid as f64 / txn.validators.len() as f64) < VALIDATOR_THRESHOLD {
                valid_data = false;
            }
        });
        
        if !valid_data {
            return Err(Self::Error {
                details: InvalidBlockErrorReason::InvalidTxns
            })
        }

        Ok(true)
    }
}