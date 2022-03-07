use crate::aggregator::Aggregator;
use crate::config::{Committee, Parameters};
use crate::error::{ConsensusError, ConsensusResult};
use crate::leader::LeaderElector;
use crate::mempool::{MempoolDriver, NodeMempool};
use crate::messages::{ProposedBlock,Block, Timeout,Status, Vote, QC, TC,SS};
use crate::synchronizer::Synchronizer;
use crate::timer::Timer;
use async_recursion::async_recursion;
use bytes::Bytes;
use crypto::Hash as _;
use crypto::{Digest, PublicKey, SignatureService};
use log::{debug, error, info, warn};
use network::NetMessage;
use serde::{Deserialize, Serialize};
use std::cmp::max;
use store::Store;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{sleep, Duration};

#[cfg(test)]
#[path = "tests/core_tests.rs"]
pub mod core_tests;

pub type RoundNumber = u64;

#[derive(Serialize, Deserialize, Debug)]
pub enum CoreMessage {
    Propose(ProposedBlock),
    // Block(Block),
    Vote(Vote),
    Timeout(Timeout),
    Status(Status),
    LoopBack(Block),
    SyncRequest(Digest, PublicKey),
}

pub struct Core<Mempool> {
    name: PublicKey,
    committee: Committee,
    parameters: Parameters,
    store: Store,
    signature_service: SignatureService,
    leader_elector: LeaderElector,
    mempool_driver: MempoolDriver<Mempool>,
    synchronizer: Synchronizer,
    core_channel: Receiver<CoreMessage>,
    network_channel: Sender<NetMessage>,
    commit_channel: Sender<Block>,
    round: RoundNumber,
    last_voted_round: RoundNumber,
    high_qc:QC,
    high_tc:TC,
    block:Block,
    timer: Timer<RoundNumber>,
    aggregator: Aggregator,
    latest_commit_digest: Option<Digest>,
}

impl<Mempool: 'static + NodeMempool> Core<Mempool> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: PublicKey,
        committee: Committee,
        parameters: Parameters,
        signature_service: SignatureService,
        store: Store,
        leader_elector: LeaderElector,
        mempool_driver: MempoolDriver<Mempool>,
        synchronizer: Synchronizer,
        core_channel: Receiver<CoreMessage>,
        network_channel: Sender<NetMessage>,
        commit_channel: Sender<Block>,
    ) -> Self {
        let aggregator = Aggregator::new(committee.clone());
        Self {
            name,
            committee,
            parameters,
            signature_service,
            store,
            leader_elector,
            mempool_driver,
            synchronizer,
            network_channel,
            commit_channel,
            core_channel,
            round: 1,
            last_voted_round: 0,
            high_qc: QC::genesis(),
            high_tc:TC::genesis(),
            block:Block::genesis(),
            timer: Timer::new(),
            aggregator,
            latest_commit_digest: Some(Block::genesis().digest()), // TODO: (1) remove option; (2) change to use Digest::default() for genesis block digest for consistency (need to change digest impl for Block)
        }
    }

    async fn store_block(&mut self, block: &Block) -> ConsensusResult<()> {
        let key = block.digest().to_vec();
        let value = bincode::serialize(block).expect("Failed to serialize block");
        self.store
            .write(key, value)
            .await
            .map_err(ConsensusError::from)
    }

    async fn schedule_timer(&mut self) {
        self.timer
            .schedule(self.parameters.timeout_delay, self.round)
            .await;
    }

    async fn transmit(
        &mut self,
        message: &CoreMessage,
        to: Option<PublicKey>,
    ) -> ConsensusResult<()> {
        let addresses = if let Some(to) = to {
            debug!("Sending {:?} to {}", message, to);
            vec![self.committee.address(&to)?]
        } else {
            debug!("Broadcasting {:?}", message);
            self.committee.broadcast_addresses(&self.name)
        };
        let bytes = bincode::serialize(message).expect("Failed to serialize core message");
        let message = NetMessage(Bytes::from(bytes), addresses);
        if let Err(e) = self.network_channel.send(message).await {
            panic!("Failed to send block through network channel: {}", e);
        }
        Ok(())
    }

    // -- Start Safety Module --
    fn increase_last_voted_round(&mut self, target: RoundNumber) {
        self.last_voted_round = max(self.last_voted_round, target);
    }

    async fn make_vote(&mut self, block: &Block) -> Vote {
        // Check if we can vote for this block.
        
        // Ensure we won't vote for contradicting blocks.
        self.increase_last_voted_round(block.round);
        // [issue #15]: Write to storage preferred_round and last_voted_round.
        Vote::new(block.clone(), self.name,self.signature_service.clone()).await
    }
    // -- End Safety Module --

    // -- Start Pacemaker --

    // async fn make_vote2(&mut self, vote1: &Vote) -> Vote {
    //     Vote::new(2, vote1.hash.clone(), vote1.round, self.name, self.signature_service.clone()).await
    // }

    fn update_high_qc(&mut self, qc: &QC) {
        if qc.round > self.high_qc.round {
            self.high_qc = qc.clone();
        }
    }

    async fn local_timeout_round(&mut self) -> ConsensusResult<()> {
        warn!("Timeout reached for round {}", self.round);
        self.increase_last_voted_round(self.round);
        let timeout = Timeout::new(
            self.block.clone(),//update self.block here
            self.name,
            self.signature_service.clone(),
        )
        .await;
        debug!("Created {:?}", timeout);
        self.schedule_timer().await;
        let message = CoreMessage::Timeout(timeout.clone());
        self.transmit(&message, None).await?;
        self.handle_timeout(&timeout).await
    }

    #[async_recursion]
    async fn handle_vote(&mut self, vote: &Vote) -> ConsensusResult<()> {
        debug!("Processing {:?}", vote);
        if vote.block.round < self.round {
            return Ok(());
        }

        // Ensure the vote is well formed.
        vote.verify(&self.committee)?;

        // Add the new vote to our aggregator and see if we have a quorum.
        if let Some(qc) = self.aggregator.add_vote(vote.clone())? {
            // vote2 qc
            debug!("Assembled vote2 qc {:?}", qc);

            if let Some(block) = self
                .synchronizer
                .get_block(&qc.hash)
                .await
                .expect("Failed to read block") {

                // Only commit qc block here, leave tc block to the process_block for committing
                

                // block in store => all ancestors should have been committed
                // we commit the block here
                self.mempool_driver.cleanup(&block).await;

                if !block.payload.is_empty() {
                    info!("Committed {}", block);

                    #[cfg(feature = "benchmark")]
                    for x in &block.payload {
                        info!("Committed B{}({})", block.round, base64::encode(x));
                    }
                }
                debug!("Committed {:?}", block);
                if let Err(e) = self.commit_channel.send(block.clone()).await {
                    warn!("Failed to send block through the commit channel: {}", e);
                }

                self.latest_commit_digest = Some(block.digest().clone());
                
                
            }

            // Process the QC.
            self.process_qc(&qc).await;
            //update highest voted block
            self.update_highest_block(&vote.block);
            // Make a new block if we are the next leader.
            if self.name == self.leader_elector.get_leader(self.round) {
                self.generate_proposal(self.high_qc.clone(),None,None).await?;
            }
        }
        Ok(())
    }

    async fn handle_timeout(&mut self, timeout: &Timeout) -> ConsensusResult<()> {
        debug!("Processing {:?}", timeout);
        if timeout.block.round < self.round {
            return Ok(());
        }

        // Ensure the timeout is well formed.
        timeout.verify(&self.committee)?;

        // Process the QC embedded in the timeout.
        // self.process_qc(&timeout.high_qc).await;

        // Add the new vote to our aggregator and see if we have a quorum.
        if let Some(tc) = self.aggregator.add_timeout(timeout.clone())? {
            debug!("Assembled tc {:?}", tc);

            // Try to advance the round.
            self.advance_round(tc.round).await;

            // Broadcast the TC.
            self.update_high_tc(&tc);

            let status = Status::new(
                self.high_qc.clone(),
                self.high_tc.clone(),
                self.round-1,
                self.name,
                self.signature_service.clone(),
            )
            .await;
            // Make a new block if we are the next leader.
            let message = CoreMessage::Status(status.clone());
            let sender:PublicKey;
            sender=self.leader_elector.get_leader(self.round);
            self.transmit(&message, Some(sender)).await?;
            self.handle_status(&status).await?;
            //let message = CoreMessage::TC(tc.clone());
            //self.transmit(&message, None).await?;

            // Make a new block if we are the next leader.
            // if self.name == self.leader_elector.get_leader(self.round) {
            //     self.generate_proposal(Some(tc)).await?;
            // }
        }
        Ok(())
    }
    async fn handle_status(&mut self, status: &Status) -> ConsensusResult<()> {
        debug!("Processing {:?}", status);
        if status.round < self.round {
            return Ok(());
        }

        // Ensure the status is well formed.
        status.verify(&self.committee)?;

        // Process the QC embedded in the timeout.
        // self.process_qc(&timeout.high_qc).await;

        // Add the new vote to our aggregator and see if we have a quorum.
        if let Some(ss )= self.aggregator.add_status(status.clone())? {
            debug!("Assembled ss {:?}", ss);

            if self.high_tc.round == ss.round {
                if self.name == self.leader_elector.get_leader(self.round) {
                    self.generate_proposal(self.high_qc.clone(),Some(self.high_tc.clone()),None).await?;
                }
            }
            else{
                if let Some(ref status)=ss.highest_status().clone(){
                // let Some(ref block)=status.high_tc.highest_tc_block().clone();

                    if self.name == self.leader_elector.get_leader(self.round) {
                        self.generate_proposal(status.high_qc.clone(),None,Some(ss)).await?;
                    }
                }
            }
        }
        Ok(())
    }

    // fn update_high_qc(&mut self, qc: &QC) {
    //     if qc.round > self.high_qc.round {
    //         self.high_qc = qc.clone();
    //     }
    // }
    fn update_highest_block(&mut self, block: &Block) {
        if block.round > self.block.round {
            self.block = block.clone();
        }
    }
    fn update_high_tc(&mut self, tc: &TC) {
        if tc.round > self.high_tc.round {
            self.high_tc = tc.clone();
        }
    }

    #[async_recursion]
    async fn advance_round(&mut self, round: RoundNumber) {
        if round < self.round {
            return;
        }
        self.timer.cancel(self.round).await;
        self.round = round + 1;
        debug!("Moved to round {}", self.round);

        // Cleanup the vote aggregator.
        self.aggregator.cleanup(&self.round);

        // Schedule a new timer for this round.
        self.schedule_timer().await;
    }
    // -- End Pacemaker --

    #[async_recursion]
    async fn generate_proposal(&mut self, qc:QC,tc:Option<TC>,ss: Option<SS>) -> ConsensusResult<()> {
        // Make a new block.
        // let qc=self.high_qc.clone();
        // if tc.is_some() {
        //     qc = None;
        // } else {
        //     qc = Some(self.high_qc_vote2.clone());
        // }
        let payload = self
            .mempool_driver
            .get(self.parameters.max_payload_size)
            .await;
        let pblock = ProposedBlock::new(
            payload,
            self.round,
            qc,
            tc,
            ss,
            self.name,
            self.signature_service.clone(),
        )
        .await;

        if !pblock.block.payload.is_empty() {
            info!("Created {}", pblock);

            #[cfg(feature = "benchmark")]
            for x in &pblock.block.payload {
                info!("Created B{}({})", pblock.block.round, base64::encode(x));
            }
        }
        debug!("Created {:?}", pblock);

        // Process our new block and broadcast it.
        let message = CoreMessage::Propose(pblock.clone());
        self.transmit(&message, None).await?;
        self.process_block(&pblock.block).await?;

        // Wait for the minimum block delay.
        sleep(Duration::from_millis(self.parameters.min_block_delay)).await;
        Ok(())
    }

    async fn process_qc(&mut self, qc: &QC) {
        // self.advance_round(qc.round).await;
        self.update_high_qc(qc);
    }

    #[async_recursion]
    async fn process_block(&mut self, block: &Block) -> ConsensusResult<()> {
        debug!("Processing {:?}", block);
        // Storing block happen if the following condition is satisfied:
        // (1) for qc blocks: if all ancestor blocks (tc and qc) have been stored
        // (2) for tc blocks: always store (following hotstuff org design, but may cause resource exhaustion)

        if self.high_tc.round+1 == block.round{
            
            self.store_block(&block).await?;
            

            // Ensure the block's round is as expected (Note it is assumed with qc/tc in block self.round is up-to-date).
            // This check is important: it prevents bad leaders from producing blocks
            // far in the future that may cause overflow on the round number.
            if block.round != self.round {
                return Ok(());
            }

            // See if we can vote for this block (vote1).
            let vote = self.make_vote(block).await; 
            debug!("Created {:?}", vote);
            let message = CoreMessage::Vote(vote.clone());
            self.transmit(&message, None).await?;
            self.handle_vote(&vote).await?;
            

            return Ok(());
        }
        // else if self.high_tc.round+1 == block.round {
        //     if ss.round+1 == pblock.block.round{
        //         self.store_block(block).await?;
        //     }

        //     if pblock.block.round != self.round {
        //         return Ok(());
        //     }

        //     // See if we can vote for this block (vote1).
        //     if let vote = self.make_vote(pblock).await {
        //         debug!("Created {:?}", vote);
        //         let message = CoreMessage::Vote(vote.clone());
        //         self.transmit(&message, None).await?;
        //         self.handle_vote(&vote).await?;
        //     }

        //     return Ok(());
        // }
        // Let's see if we have the ancestors of the block, that is:
        //      b_n+2 <- |qc1; b_n+1| <- |tc_n; bn| <- .... <- |qc0; block|
        // If we don't, the synchronizer asks for them to other nodes. It will
        // then ensure we process all ancestors in the correct order, and
        // finally make us resume processing this block.
        let mut ancestors = Vec::new();
        let mut iter_block = block.clone();

        let pre_iter_block = match self.synchronizer.get_parent(&iter_block, &block).await? {
            Some(pre_iter_block) => pre_iter_block,
            None => {
                debug!("Processing of {} suspended: missing parent", iter_block.digest());
                return Ok(());
            }
        };

        ancestors.push(pre_iter_block.clone());

        // if pre_iter_block.tc.is_some() {
        //     iter_block = pre_iter_block;

        //     while iter_block.tc.is_some() {
        //         let pre_iter_block = match self.synchronizer.get_parent(&iter_block, &block).await? {
        //             Some(pre_iter_block) => pre_iter_block,
        //             None => {
        //                 debug!("Processing of {} suspended: missing parent", iter_block.digest());
        //                 return Ok(());
        //             }
        //         };

        //         ancestors.push(pre_iter_block.clone());
        //         iter_block = pre_iter_block;
        //     }
        // }

        // Ancestors vector has all ancestor blocks in reverse order (start: newest -> end: oldest)
        // Store the block only if we have already processed all its ancestors.
        // Don't store if the round doesn't match up to prevent DOS
        
        if self.high_qc.round + 1 == block.round {
            self.store_block(&block).await?;
        }
        // } else {
        //     debug!("Invalid block: {:?}", pblock);
        //     assert!(false, "Invalid block");
        // }

        // Cleanup the mempool.
        for b in ancestors.iter() {
            self.mempool_driver.cleanup(block).await;
        }

        // We can commit all blocks in ancestors, starting from the end.
        // Note that we commit blocks only if we have all its ancestors.
        ancestors.reverse();
        for b in ancestors.iter() {
            if let Some(ref latest_commit_digest) = self.latest_commit_digest {
                if *latest_commit_digest == b.digest() {
                    continue; // ignore committed block (committed during handle_vote)
                }
            }

            if !b.payload.is_empty() {
                info!("Committed {}", b);

                #[cfg(feature = "benchmark")]
                for x in &b.payload {
                    info!("Committed B{}({})", b.round, base64::encode(x));
                }
            }
            debug!("Committed {:?}", b);
            if let Err(e) = self.commit_channel.send(b.clone()).await {
                warn!("Failed to send block through the commit channel: {}", e);
            }

            self.latest_commit_digest = Some(b.digest().clone());
        }

        // Ensure the block's round is as expected.
        if block.round != self.round {
            return Ok(());
        }

        // See if we can vote for this block (vote).
        let vote = self.make_vote(block).await;
        debug!("Created {:?}", vote);
        let message = CoreMessage::Vote(vote.clone());
        self.transmit(&message, None).await?;
        self.handle_vote(&vote).await?;
    
        Ok(())
    }

    async fn handle_proposal(&mut self, pblock: &ProposedBlock) -> ConsensusResult<()> {
        let digest = pblock.digest();

        // Ensure the block proposer is the right leader for the round.
        ensure!(
            pblock.author == self.leader_elector.get_leader(pblock.block.round),
            ConsensusError::WrongLeader {
                digest,
                leader: pblock.author,
                round: pblock.block.round
            }
        );
        ensure!(
            pblock.author == pblock.block.author,
            ConsensusError::LeaderMisMatch
        );
        ensure!(
            self.high_qc.round +1 == pblock.block.round,
            ConsensusError::BlockExtendError
        );
        pblock.verify(&self.committee)?;
        // Check the block is correctly formed.
        pblock.block.verify(&self.committee)?;
        let safety_rule_1 = pblock.block.round > self.last_voted_round;

        let safety_rule_2 = pblock.qc.round +1 == pblock.block.round;
        let mut safety_rule_3 = false;
        if let Some(ref tc) = pblock.tc {
            safety_rule_3 = tc.round + 1 == pblock.block.round;
        } 
        else if let Some(ref ss) = pblock.ss {
            safety_rule_3 = ss.round + 1 == pblock.block.round;
        }
        if !(safety_rule_1 && safety_rule_2 && safety_rule_3) {
            ConsensusError::SafetyRuleViolated;
        }

            // Process the QC (if any).
        // self.process_qc(pblock.qc).await;
        // if let Some(ref tc) = pblock.tc {
        //     // Process the TC (if any). This may allow us to advance round.
        //     self.advance_round(tc.round).await;
        // }
        // else{

        // }
        // Let's see if we have the block's data. If we don't, the mempool
        // will get it and then make us resume processing this block.
        if !self.mempool_driver.verify(&pblock.block).await? {
            debug!("Processing of {} suspended: missing payload", digest);
            return Ok(());
        }

        // All check pass, we can process this block.
        self.process_block(&pblock.block).await
    }

    async fn handle_sync_request(
        &mut self,
        digest: Digest,
        sender: PublicKey,
    ) -> ConsensusResult<()> {
        if let Some(bytes) = self.store.read(digest.to_vec()).await? {
            let block = bincode::deserialize(&bytes)?;
            let message = CoreMessage::Propose(block);
            self.transmit(&message, Some(sender)).await?;
        }
        Ok(())
    }

    //async fn handle_tc(&mut self, tc: TC) -> ConsensusResult<()> {
    //    self.advance_round(tc.round).await;
    //    if self.name == self.leader_elector.get_leader(self.round) {
    //        self.generate_proposal(Some(tc)).await?;
    //    }
    //    Ok(())
    //}

    pub async fn run(&mut self) {
        // Upon booting, generate the very first block (if we are the leader).
        // Also, schedule a timer in case we don't hear from the leader.
        self.schedule_timer().await;
        if self.name == self.leader_elector.get_leader(self.round) {
            self.generate_proposal(QC::genesis(),None,None)
                .await
                .expect("Failed to send the first block");
        }

        // This is the main loop: it processes incoming blocks and votes,
        // and receive timeout notifications from our Timeout Manager.
        loop {
            let result = tokio::select! {
                Some(message) = self.core_channel.recv() => {
                    match message {
                        CoreMessage::Propose(pblock) => self.handle_proposal(&pblock).await,
                        CoreMessage::Vote(vote) => self.handle_vote(&vote).await,
                        CoreMessage::Timeout(timeout) => self.handle_timeout(&timeout).await,
                        //CoreMessage::TC(tc) => self.handle_tc(tc).await,
                        CoreMessage::Status(status) => self.handle_status(&status).await,
                        CoreMessage::LoopBack(block) => self.process_block(&block).await,
                        CoreMessage::SyncRequest(digest, sender) => self.handle_sync_request(digest, sender).await
                    }
                },
                // Some(_) = self.timer.notifier.recv() => self.local_timeout_round().await,
                else => break,
            };
            match result {
                Ok(()) => (),
                Err(ConsensusError::StoreError(e)) => error!("{}", e),
                Err(ConsensusError::SerializationError(e)) => error!("Store corrupted. {}", e),
                Err(e) => warn!("{}", e),
            }
        }
    }
}