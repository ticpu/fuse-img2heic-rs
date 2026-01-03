use anyhow::Result;
use crossbeam::channel::{self, Sender};
use log::{debug, error, info, trace};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::cache::{create_cache_key_and_context_for_path, ImageCache};
use crate::config::HeicSettings;

pub struct ConversionJob {
    pub input_path: PathBuf,
    pub heic_settings: HeicSettings,
    pub result_sender: Option<mpsc::Sender<Result<Vec<u8>>>>,
}

pub struct ConversionThreadPool {
    sender: Option<Sender<ConversionJob>>,
    workers: Vec<thread::JoinHandle<()>>,
    cache: Arc<ImageCache>,
}

impl ConversionThreadPool {
    pub fn new(num_workers: usize, cache: Arc<ImageCache>) -> Self {
        let (sender, receiver) = channel::unbounded::<ConversionJob>();
        let receiver = Arc::new(receiver);

        info!("Starting {num_workers} conversion worker threads");

        let mut workers = Vec::with_capacity(num_workers);

        for id in 0..num_workers {
            let receiver = Arc::clone(&receiver);
            let cache = Arc::clone(&cache);

            let handle = thread::spawn(move || {
                trace!("Worker {id} started");

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

                            // Always cache the result
                            let original_size = std::fs::metadata(&job.input_path)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            let (cache_key, context) = create_cache_key_and_context_for_path(
                                &job.input_path,
                                original_size,
                                &job.heic_settings,
                            );
                            if let Err(e) = cache.put_with_context(cache_key, data.clone(), &context) {
                                debug!("Worker {id} failed to cache result: {e}");
                            }

                            // Send result if someone's waiting
                            if let Some(sender) = job.result_sender {
                                let _ = sender.send(Ok(data));
                            }
                        }
                        Err(e) => {
                            error!(
                                "Worker {} conversion failed for {:?}: {}",
                                id, job.input_path, e
                            );
                            if let Some(sender) = job.result_sender {
                                let _ = sender.send(Err(e));
                            }
                        }
                    }
                }

                debug!("Worker {id} shutting down");
            });

            workers.push(handle);
        }

        Self {
            sender: Some(sender),
            workers,
            cache,
        }
    }

    pub fn submit_job(&self, job: ConversionJob) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Thread pool is shut down"))?
            .send(job)
            .map_err(|_| anyhow::anyhow!("Failed to submit conversion job - thread pool shut down"))
    }

    pub fn convert_image_blocking(
        &self,
        input_path: PathBuf,
        heic_settings: HeicSettings,
    ) -> Result<Vec<u8>> {
        let (result_sender, result_receiver) = mpsc::channel();

        let job = ConversionJob {
            input_path,
            heic_settings,
            result_sender: Some(result_sender),
        };

        self.submit_job(job)?;

        result_receiver
            .recv()
            .map_err(|_| anyhow::anyhow!("Conversion job was cancelled"))?
    }

    /// Submit a file for background conversion (prefetch). Result will be cached.
    pub fn prefetch(&self, input_path: PathBuf, heic_settings: HeicSettings) {
        // Check if already cached
        let original_size = std::fs::metadata(&input_path).map(|m| m.len()).unwrap_or(0);
        let (cache_key, context) = create_cache_key_and_context_for_path(
            &input_path,
            original_size,
            &heic_settings,
        );
        if self.cache.get_with_context(&cache_key, &context).is_some() {
            return; // Already cached
        }

        let job = ConversionJob {
            input_path,
            heic_settings,
            result_sender: None, // No one waiting, just cache it
        };

        let _ = self.submit_job(job); // Ignore errors for prefetch
    }
}

impl Drop for ConversionThreadPool {
    fn drop(&mut self) {
        info!("Shutting down conversion thread pool");

        // Close the sender to signal workers to stop
        drop(self.sender.take());

        // Wait for all workers to finish
        while let Some(worker) = self.workers.pop() {
            if let Err(e) = worker.join() {
                error!("Worker thread panicked: {e:?}");
            }
        }

        info!("All conversion workers shut down");
    }
}
