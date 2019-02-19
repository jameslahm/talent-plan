use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use labrpc;

use crate::raft;
use crate::raft::persister::*;
use kvraft::{
    client,
    errors::{Error, Result},
    server, service,
};
use rand::Rng;

static ID: AtomicUsize = AtomicUsize::new(0);

fn uniqstring() -> String {
    format!("{}", ID.fetch_add(1, Ordering::Relaxed))
}

pub struct Config {
    net: labrpc::Network,
    n: usize,
    kvservers: Vec<Option<server::Node>>,
    saved: Vec<Arc<SimplePersister>>,
    // the port file names each sends to
    endnames: Vec<Vec<String>>,
    clerks: HashMap<String, Vec<String>>,
    next_client_id: usize,
    maxraftstate: u64,

    // time at which make_config() was called
    start: Instant,

    // begin()/end() statistics

    // time at which test_test.go called cfg.begin()
    t0: Instant,
    // rpc_total() at start of test
    rpcs0: usize,
    // number of agreements
    ops: AtomicUsize,
}

impl Config {
    pub fn new(n: usize, unreliable: bool, maxraftstate: u64) -> Config {
        let mut cfg = Config {
            net: labrpc::Network::new(),
            n,
            kvservers: vec![None; n],
            saved: (0..n).map(|_| Arc::new(SimplePersister::new())).collect(),
            endnames: vec![vec![String::new(); n]; n],
            clerks: HashMap::new(),
            // client ids start 1000 above the highest serverid,
            next_client_id: n + 1000,
            maxraftstate,
            start: Instant::now(),
            t0: Instant::now(),
            rpcs0: 0,
            ops: AtomicUsize::new(0),
        };

        // create a full set of KV servers.
        for i in 0..cfg.n {
            cfg.start_server(i);
        }

        cfg.connect_all();

        cfg.net.set_reliable(!unreliable);

        cfg
    }

    fn rpc_total(&self) -> usize {
        self.net.total_count()
    }

    fn check_timeout(&self) {
        // enforce a two minute real-time limit on each test
        if self.start.elapsed() > Duration::from_secs(120) {
            panic!("test took longer than 120 seconds");
        }
    }

    // Maximum log size across all servers
    pub fn log_size(&self) -> usize {
        let mut logsize = 0;
        for save in &self.saved {
            let n = save.raft_state().len();
            if n > logsize {
                logsize = n;
            }
        }
        logsize
    }

    // Maximum snapshot size across all servers
    pub fn snapshot_size(&self) -> usize {
        let mut snapshotsize = 0;
        for save in &self.saved {
            let n = save.snapshot().len();
            if n > snapshotsize {
                snapshotsize = n;
            }
        }
        snapshotsize
    }

    // attach server i to servers listed in to
    // caller must hold cfg.mu
    fn connect(&self, i: usize, to: &[usize]) {
        debug!("connect peer {} to {:?}", i, to);

        // outgoing socket files
        for j in to {
            let endname = &self.endnames[i][*j];
            self.net.enable(endname, true);
        }

        // incoming socket files
        for j in to {
            let endname = &self.endnames[*j][i];
            self.net.enable(endname, true);
        }
    }

    // detach server i from the servers listed in from
    // caller must hold cfg.mu
    fn disconnect(&self, i: usize, from: &[usize]) {
        debug!("disconnect peer {} from {:?}", i, from);

        // outgoing socket files
        for j in from {
            if !self.endnames[i].is_empty() {
                let endname = &self.endnames[i][*j];
                self.net.enable(endname, false);
            }
        }

        // incoming socket files
        for j in from {
            if !self.endnames[*j].is_empty() {
                let endname = &self.endnames[*j][i];
                self.net.enable(endname, false);
            }
        }
    }

    pub fn all(&self) -> Vec<usize> {
        (0..self.n).collect()
    }

    pub fn connect_all(&self) {
        for i in 0..self.n {
            self.connect(i, &self.all());
        }
    }

    // Sets up 2 partitions with connectivity between servers in each  partition.
    fn partition(&self, p1: &[usize], p2: &[usize]) {
        debug!("partition servers into: {:?} {:?}", p1, p2);
        for i in p1 {
            self.disconnect(*i, p2);
            self.connect(*i, p1);
        }
        for i in p2 {
            self.disconnect(*i, p1);
            self.connect(*i, p2);
        }
    }

    // Create a clerk with clerk specific server names.
    // Give it connections to all of the servers, but for
    // now enable only connections to servers in to[].
    fn make_client(&mut self, to: &[usize]) -> client::Clerk {
        // a fresh set of ClientEnds.
        let mut ends = Vec::with_capacity(self.n);
        let mut endnames = Vec::with_capacity(self.n);
        for j in 0..self.n {
            let name = uniqstring();
            endnames.push(name.clone());
            let cli = self.net.create_client(name.clone());
            ends.push(service::KvClient::new(cli));
            self.net.connect(&name, &format!("{}", j));
        }

        rand::thread_rng().shuffle(&mut ends);
        let ck_name = uniqstring();
        let ck = client::Clerk::new(ck_name.clone(), ends);
        self.clerks.insert(ck_name, endnames);
        self.next_client_id += 1;
        self.connect_client(&ck, to);
        ck
    }

    fn delete_client(&mut self, ck: &client::Clerk) {
        // Remove???
        //
        // let v = &self.clerks[&ck.name];
        // for i := 0; i < len(v); i++ {
        // 	os.Remove(v[i])
        // }
        self.clerks.remove(&ck.name);
    }

    // caller should hold cfg.mu
    pub fn connect_client(&self, ck: &client::Clerk, to: &[usize]) {
        debug!("connect_client {:?} to {:?}", ck, to);
        let endnames = &self.clerks[&ck.name];
        for j in to {
            let s = &endnames[*j];
            self.net.enable(s, true);
        }
    }

    // caller should hold cfg.mu
    pub fn disconnect_client(&self, ck: &client::Clerk, from: &[usize]) {
        debug!("DisconnectClient {:?} from {:?}", ck, from);
        let endnames = &self.clerks[&ck.name];
        for j in from {
            let s = &endnames[*j];
            self.net.enable(s, false);
        }
    }

    // Shutdown a server by isolating it
    pub fn shutdown_server(&mut self, i: usize) {
        self.disconnect(i, &self.all());

        // disable client connections to the server.
        // it's important to do this before creating
        // the new Persister in saved[i], to avoid
        // the possibility of the server returning a
        // positive reply to an Append but persisting
        // the result in the superseded Persister.
        self.net.delete_server(&format!("{}", i));

        // a fresh persister, in case old instance
        // continues to update the Persister.
        // but copy old persister's content so that we always
        // pass Make() the last persisted state.
        let p = raft::persister::SimplePersister::new();
        p.save_state_and_snapshot(self.saved[i].raft_state(), self.saved[i].snapshot());
        self.saved[i] = Arc::new(p);

        if let Some(kv) = self.kvservers[i].take() {
            kv.kill();
        }
    }

    // If restart servers, first call shutdown_server
    pub fn start_server(&mut self, i: usize) {
        // a fresh set of outgoing ClientEnd names.
        self.endnames[i] = (0..self.n).map(|_| uniqstring()).collect();

        // a fresh set of ClientEnds.
        let mut ends = Vec::with_capacity(self.n);
        for (j, name) in self.endnames[i].iter().enumerate() {
            let cli = self.net.create_client(name.clone());
            ends.push(raft::service::RaftClient::new(cli));
            self.net.connect(name, &format!("{}", j));
        }

        // a fresh persister, so old instance doesn't overwrite
        // new instance's persisted state.
        // give the fresh persister a copy of the old persister's
        // state, so that the spec is that we pass StartKVServer()
        // the last persisted state.
        let sp = raft::persister::SimplePersister::new();
        sp.save_state_and_snapshot(self.saved[i].raft_state(), self.saved[i].snapshot());
        let p = Arc::new(sp);
        self.saved[i] = p.clone();

        let kv = server::KvServer::new(ends, i, Box::new(p), self.maxraftstate);
        let rf_node = kv.rf.clone();
        let kv_node = server::Node::new(kv);
        self.kvservers[i] = Some(kv_node.clone());

        let mut builder = labrpc::ServerBuilder::new(format!("{}", i));
        raft::service::add_raft_service(rf_node, &mut builder);
        service::add_kv_service(kv_node, &mut builder);
        let srv = builder.build();
        self.net.add_server(srv);
    }

    pub fn leader(&self) -> Result<usize> {
        for (i, kv) in self.kvservers.iter().enumerate() {
            if let Some(kv) = kv {
                if kv.is_leader() {
                    return Ok(i);
                }
            }
        }
        Err(Error::NoLeader)
    }

    // Partition servers into 2 groups and put current leader in minority
    fn make_partition(&self) -> (Vec<usize>, Vec<usize>) {
        let l = self.leader().unwrap_or(0);
        let mut p1 = Vec::with_capacity(self.n / 2 + 1);
        let mut p2 = Vec::with_capacity(self.n / 2);
        for i in 0..self.n {
            if i != l {
                if p1.len() + 1 < self.n / 2 + 1 {
                    p1.push(i);
                } else {
                    p2.push(i);
                }
            }
        }
        p2.push(l);
        (p1, p2)
    }

    // start a Test.
    // print the Test message.
    // e.g. cfg.begin("Test (2B): RPC counts aren't too high")
    pub fn begin(&mut self, description: &str) {
        info!("{} ...", description);
        self.t0 = Instant::now();
        self.rpcs0 = self.rpc_total();
        self.ops.store(0, Ordering::Relaxed);
    }

    // end a Test -- the fact that we got here means there
    // was no failure.
    // print the Passed message,
    // and some performance numbers.
    pub fn end(&mut self) {
        self.check_timeout();

        // real time
        let t = self.t0.elapsed();
        // number of Raft peers
        let npeers = self.n;
        // number of RPC sends
        let nrpc = self.rpc_total() - self.rpcs0;
        // number of clerk get/put/append calls
        let nops = self.ops.load(Ordering::Relaxed);

        info!("  ... Passed --");
        info!("  {:?}  {} {} {}", t, npeers, nrpc, nops);
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        for s in &self.kvservers {
            if let Some(s) = s {
                s.kill();
            }
        }
        self.check_timeout();
    }
}
