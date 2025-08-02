use anyhow::Result;
use crossbeam::channel::{self, Sender};
use log::{debug, error, info};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::oneshot;

use crate::config::HeicSettings;

#[derive(Debug)]
pub struct ConversionJob {
    pub input_path: PathBuf,
    pub heic_settings: HeicSettings,
    pub result_sender: oneshot::Sender<Result<Vec<u8>>>,
}

pub struct ConversionThreadPool {
    sender: Sender<ConversionJob>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl ConversionThreadPool {
    pub fn new(num_workers: usize) -> Self {
        let (sender, receiver) = channel::unbounded::<ConversionJob>();
        let receiver = Arc::new(receiver);

        info!("Starting {num_workers} conversion worker threads");

        let mut workers = Vec::with_capacity(num_workers);

        for id in 0..num_workers {
            let receiver = Arc::clone(&receiver);

            let handle = thread::spawn(move || {
                debug!("Worker {id} started");

                while let Ok(job) = receiver.recv() {
                    debug!("Worker {} processing job for: {:?}", id, job.input_path);

                    let result = crate::image_converter::convert_to_heic_blocking(
                        &job.input_path,
                        &job.heic_settings,
                    );

                    match result {
                        Ok(data) => {
                            debug!(
                                "Worker {} successfully converted: {:?} ({} bytes)",
                                id,
                                job.input_path,
                                data.len()
                            );
                            if job.result_sender.send(Ok(data)).is_err() {
                                debug!("Worker {id} failed to send result - receiver dropped");
                            }
                        }
                        Err(e) => {
                            error!(
                                "Worker {} conversion failed for {:?}: {}",
                                id, job.input_path, e
                            );
                            if job.result_sender.send(Err(e)).is_err() {
                                debug!("Worker {id} failed to send error - receiver dropped");
                            }
                        }
                    }
                }

                debug!("Worker {id} shutting down");
            });

            workers.push(handle);
        }

        Self { sender, workers }
    }

    pub fn submit_job(&self, job: ConversionJob) -> Result<()> {
        self.sender
            .send(job)
            .map_err(|_| anyhow::anyhow!("Failed to submit conversion job - thread pool shut down"))
    }

    pub fn convert_image_blocking(
        &self,
        input_path: PathBuf,
        heic_settings: HeicSettings,
    ) -> Result<Vec<u8>> {
        let (result_sender, result_receiver) = oneshot::channel();

        let job = ConversionJob {
            input_path,
            heic_settings,
            result_sender,
        };

        self.submit_job(job)?;

        // Use blocking wait with a runtime
        let rt = tokio::runtime::Handle::try_current()
            .or_else(|_| tokio::runtime::Runtime::new().map(|rt| rt.handle().clone()))?;

        rt.block_on(result_receiver)
            .map_err(|_| anyhow::anyhow!("Conversion job was cancelled"))?
    }
}

impl Drop for ConversionThreadPool {
    fn drop(&mut self) {
        info!("Shutting down conversion thread pool");

        // Close the sender to signal workers to stop
        drop(self.sender.clone());

        // Wait for all workers to finish
        while let Some(worker) = self.workers.pop() {
            if let Err(e) = worker.join() {
                error!("Worker thread panicked: {e:?}");
            }
        }

        info!("All conversion workers shut down");
    }
}
