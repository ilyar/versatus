use std::collections::BTreeMap;

use crossbeam_channel::{unbounded, Receiver, Sender};
use dashmap::DashMap;
use events::{QuorumCertifiedTxn, Vote, VoteReceipt};
use indexmap::IndexMap;
use job_scheduler::JobScheduler;
use mempool::TxnRecord;
use primitives::{
    base::PeerId as PeerID,
    ByteVec,
    FarmerQuorumThreshold,
    HarvesterQuorumThreshold,
    QuorumType,
    RawSignature,
};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use signer::signer::{SignatureProvider, Signer};
use tracing::error;
use validator::{
    txn_validator::{StateSnapshot, TxnFees},
    validator_core_manager::ValidatorCoreManager,
};
use vrrb_core::{
    bloom::Bloom,
    txn::{TransactionDigest, Txn},
};


/// `JobSchedulerController` is a struct that contains a `JobScheduler`, a
/// `Receiver<Job>` for synchronous jobs, a `Sender<JobResult>` for synchronous
/// jobs, a `Receiver<Job>` for asynchronous jobs, a `Sender<JobResult>` for
/// asynchronous jobs, a `ValidatorCoreManager`, and a `StateSnapshot`.
///
/// Properties:
///
/// * `job_scheduler`: The JobScheduler struct that we created earlier.
/// * `sync_jobs_receiver`: Receiver<Job>
/// * `sync_jobs_outputs_sender`: Sender<JobResult>
/// * `async_jobs_receiver`: Receiver<Job>
/// * `async_jobs_outputs_sender`: Sender<JobResult>
/// * `validator_core_manager`: This is the validator core manager that we
///   created in the previous
/// section.
/// * `state_snapshot`: A reference to the state snapshot that the job scheduler
///   will use to execute
/// jobs.
pub struct JobSchedulerController<'a> {
    pub job_scheduler: JobScheduler,
    sync_jobs_receiver: Receiver<Job>,
    sync_jobs_outputs_sender: Sender<JobResult>,
    async_jobs_receiver: Receiver<Job>,
    async_jobs_outputs_sender: Sender<JobResult>,
    pub validator_core_manager: ValidatorCoreManager,
    pub state_snapshot: &'a StateSnapshot,
}

pub enum Job {
    Farm(
        (
            Vec<(TransactionDigest, TxnRecord)>,
            ByteVec,
            u16,
            ByteVec,
            SignatureProvider,
            FarmerQuorumThreshold,
        ),
    ),
    CertifyTxn(
        (
            SignatureProvider,
            Vec<Vote>,
            TransactionDigest,
            String,
            Vec<u8>,
            Txn,
        ),
    ),
}

#[derive(Debug)]
pub enum JobResult {
    Votes((Vec<Option<Vote>>, FarmerQuorumThreshold)),
    CertifiedTxn(
        Vec<Vote>,
        RawSignature,
        TransactionDigest,
        String,
        Vec<u8>,
        Txn,
    ),
}

impl<'a> JobSchedulerController<'a> {
    pub fn new(
        peer_id: PeerID,
        sync_jobs_receiver: Receiver<Job>,
        async_jobs_receiver: Receiver<Job>,
        sync_jobs_outputs_sender: Sender<JobResult>,
        async_jobs_outputs_sender: Sender<JobResult>,
        validator_core_manager: ValidatorCoreManager,
        state_snapshot: &'a StateSnapshot,
    ) -> Self {
        Self {
            job_scheduler: JobScheduler::new(peer_id),
            sync_jobs_receiver,
            async_jobs_receiver,
            sync_jobs_outputs_sender,
            async_jobs_outputs_sender,
            validator_core_manager,
            state_snapshot,
        }
    }

    pub fn execute_sync_jobs(&mut self) {
        loop {
            match self.sync_jobs_receiver.try_recv() {
                Ok(job) => match job {
                    Job::Farm((
                        txns,
                        receiver_farmer_id,
                        farmer_node_id,
                        quorum_public_key,
                        sig_provider,
                        farmer_quorum_threshold,
                    )) => {
                        let transactions: Vec<Txn> = txns.iter().map(|x| x.1.txn.clone()).collect();
                        let validated_txns: Vec<_> = self
                            .validator_core_manager
                            .validate(self.state_snapshot, transactions)
                            .into_iter()
                            .collect();
                        let backpressure = self.job_scheduler.calculate_back_pressure();
                        //Delegation Principle need to be done
                        let votes_result = self
                            .job_scheduler
                            .get_local_pool()
                            .run_sync_job(move || {
                                let votes = validated_txns
                                    .par_iter()
                                    .map_with(
                                        receiver_farmer_id,
                                        |receiver_farmer_id: &mut Vec<u8>, txn| {
                                            let mut vote = None;
                                            let txn = txn.0.clone();
                                            if let Ok(txn_bytes) = bincode::serialize(&txn) {
                                                if let Ok(signature) = sig_provider
                                                    .generate_partial_signature(txn_bytes)
                                                {
                                                    vote = Some(Vote {
                                                        farmer_id: receiver_farmer_id.clone(),
                                                        farmer_node_id,
                                                        signature,
                                                        txn,
                                                        quorum_public_key: quorum_public_key
                                                            .clone(),
                                                        quorum_threshold: 2,
                                                        execution_result: None,
                                                    });
                                                }
                                            }
                                            vote
                                        },
                                    )
                                    .collect::<Vec<Option<Vote>>>();
                                votes
                            })
                            .join();
                        if let Ok(votes) = votes_result {
                            let _ = self
                                .sync_jobs_outputs_sender
                                .send(JobResult::Votes((votes, farmer_quorum_threshold)));
                        }
                    },
                    Job::CertifyTxn((
                        sig_provider,
                        votes,
                        txn_id,
                        farmer_quorum_key,
                        farmer_id,
                        txn,
                    )) => {
                        let mut sig_shares = BTreeMap::new();
                        for v in votes.iter() {
                            sig_shares.insert(v.farmer_node_id, v.signature.clone());
                        }
                        let validated_txns: Vec<_> = self
                            .validator_core_manager
                            .validate(self.state_snapshot, vec![txn.clone()])
                            .into_iter()
                            .collect();
                        let validated = validated_txns.par_iter().any(|x| x.0.id() == txn.id());
                        if validated {
                            let result = sig_provider.generate_quorum_signature(sig_shares.clone());
                            if let Ok(threshold_signature) = result {
                                let _ =
                                    self.sync_jobs_outputs_sender.send(JobResult::CertifiedTxn(
                                        votes.clone(),
                                        threshold_signature,
                                        txn_id.clone(),
                                        farmer_quorum_key.clone(),
                                        farmer_id.clone(),
                                        txn.clone(),
                                    ));
                            } else {
                                error!("Quorum signature generation failed");
                            }
                        } else {
                            error!("Penalize Farmer for wrong votes by sending Wrong Vote event to CR Quorum");
                        }
                    },
                },
                Err(_) => {},
            }
        }
    }
}