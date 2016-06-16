// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::marker::PhantomData;
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;

use dispatcher::Dispatcher;

pub struct Supervisor<T>
    where T: Dispatcher
{
    config: Arc<RwLock<T::Config>>,
    workers: Vec<mpsc::Receiver<()>>,
    _marker: PhantomData<T>,
}

impl<T> Supervisor<T>
    where T: Dispatcher + 'static
{
    // JW TODO: this should take a struct that implements "application config"
    pub fn new(config: Arc<RwLock<T::Config>>) -> Self {
        Supervisor {
            config: config,
            workers: vec![],
            _marker: PhantomData,
        }
    }

    /// Start the supervisor and block until all workers are ready.
    pub fn start(mut self, worker_count: usize) -> super::Result<()> {
        try!(self.init(worker_count));
        debug!("Supervisor ready");
        self.run(worker_count)
    }

    // Initialize worker pool blocking until all workers are started and ready to begin processing
    // requests.
    fn init(&mut self, worker_count: usize) -> super::Result<()> {
        for worker_id in 0..worker_count {
            try!(self.spawn_worker(worker_id));
        }
        Ok(())
    }

    fn run(mut self, worker_count: usize) -> super::Result<()> {
        thread::spawn(move || {
            loop {
                for i in 0..worker_count {
                    match self.workers[i].try_recv() {
                        Err(mpsc::TryRecvError::Disconnected) => {
                            info!("Worker[{}] restarting...", i);
                            self.spawn_worker(i).unwrap();
                        }
                        Ok(msg) => warn!("Worker[{}] sent unexpected msg: {:?}", i, msg),
                        Err(mpsc::TryRecvError::Empty) => continue,
                    }
                }
                // JW TODO: switching to zmq from channels will allow us to call select across
                // multiple queues and avoid sleeping
                thread::sleep(Duration::from_millis(500));
            }
        });
        Ok(())
    }

    fn spawn_worker(&mut self, worker_id: usize) -> super::Result<()> {
        let cfg = self.config.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut worker = T::new(cfg);
        thread::spawn(move || {
            try!(worker.init());
            worker.start(tx)
        });
        if rx.recv().is_ok() {
            debug!("Worker[{}] ready", worker_id);
            self.workers.push(rx);
        } else {
            error!("Worker[{}] failed to start", worker_id);
            self.workers.remove(worker_id);
        }
        Ok(())
    }
}