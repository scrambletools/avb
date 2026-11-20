//! Uploading an image, such as new firmware, to an entity's memory object
//! as IEEE 1722.1-2021 Annex D has it: START_OPERATION UPLOAD with the
//! image's length, its octets in order in Address Access WRITEs from the
//! object's start address, then START_OPERATION STORE, or
//! STORE_AND_REBOOT to restart the entity on it, whose OPERATION_STATUS
//! reports say how storing went.
//!
//! An [`Upload`] takes a [`Controller`] through it. Like the controller,
//! it does no I/O and keeps no clock: the caller hands it the commands
//! that finished, the OPERATION_STATUS reports and the entities gone, and
//! calls [`Upload::handle_timeout`] by [`Upload::poll_timeout`].
//!
//! The entity is locked against other controllers' changes for the
//! upload, and the lock taken again every 30 s, as Milan entities let one
//! go after 60; an entity that cannot be locked takes the upload
//! unlocked. Some entities stop their streams for an upload, which they
//! refuse unlocked while the streams run.

use alloc::vec::Vec;
use core::time::Duration;

use crate::EntityId;
use crate::aecp::{AaStatus, AaTlv, AddressAccessPdu, AemStatus};
use crate::controller::{CommandId, Controller, OperationStatus, Outcome, Refusal};
use crate::descriptor::{DescriptorType, MemoryObjectDescriptor, MemoryObjectOperation};
use crate::time::Instant;

/// The most octets one WRITE carries: the 512 octets of a command's TLVs
/// less one TLV's header.
pub const MOST_PER_WRITE: usize = AddressAccessPdu::MAX_TLVS_LEN - AaTlv::HEADER_LEN;

/// How often the lock is taken again while the upload goes on.
const LOCK_AGAIN: Duration = Duration::from_secs(30);

/// How many times a WRITE goes out before the upload fails for want of an
/// answer, each with the controller's own retry: an entity may hold a
/// write up to 2 s while it erases ahead of the image. It takes a WRITE
/// again of the octets it took last without writing them twice.
const WRITE_SENDS: u8 = 8;

/// How long the entity has to say how storing went.
const STORE_TIME: Duration = Duration::from_secs(120);

/// Where an upload has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Locking the entity against other controllers' changes.
    Locking,
    /// Asking the entity to take an upload.
    Starting,
    /// Writing the image: `sent` octets of `total` taken.
    Writing {
        sent: u64,
        total: u64,
    },
    /// Asking the entity to stop the upload.
    Aborting,
    /// Storing the image, and how far, in tenths of a percent, once the
    /// entity says.
    Storing {
        progress: Option<u16>,
    },
    /// Stored. With a restart asked, the entity restarts on the image.
    Done,
    /// Stopped by the caller before storing.
    Aborted,
    Failed(Failure),
}

/// Why an upload failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The image is empty or longer than the memory object holds.
    DoesNotFit,
    /// Another controller holds the entity's lock, or has acquired it.
    Locked,
    /// The entity refused a step or did not answer it.
    Refused(Step, Outcome),
    /// The entity refused the octets written at `offset`.
    Write { offset: u64, status: AaStatus },
    /// The entity reported the operation failed, as when the image fails
    /// its check.
    Rejected,
    /// The entity went away before the image was stored.
    Gone,
    /// The entity did not say in time how storing went.
    NoStatus,
}

/// A step of an upload, as [`Failure::Refused`] names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Lock,
    Upload,
    Write,
    Store,
}

/// What a command in flight asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Lock,
    Upload,
    /// A WRITE of this many octets.
    Write(usize),
    Store,
    Abort,
}

/// An image going to an entity's memory object.
#[derive(Debug)]
pub struct Upload {
    entity_id: EntityId,
    index: u16,
    address: u64,
    image: Vec<u8>,
    /// The most octets a WRITE carries.
    segment: usize,
    restart: bool,
    stage: Stage,
    /// The command waited for, and what it asked.
    pending: Option<(CommandId, Asked)>,
    /// The lock taken again, and its letting go at the end, whose answers
    /// are not waited for.
    relock: Option<CommandId>,
    unlock: Option<CommandId>,
    locked: bool,
    locked_at: Instant,
    /// The operation_ids the entity gave the upload and the store.
    upload_id: Option<u16>,
    store_id: Option<u16>,
    /// The octets the entity has taken.
    sent: usize,
    /// How many times the WRITE from `sent` has gone out.
    sends: u8,
    /// The caller asked to stop.
    abort: bool,
    /// When the entity must have said how storing went.
    deadline: Option<Instant>,
}

impl Upload {
    /// Starts uploading `image` to `object`, one of `entity_id`'s memory
    /// objects, to be stored at the end and, with `restart`, the entity
    /// restarted on it. A WRITE carries [`MOST_PER_WRITE`] octets at most,
    /// or fewer where the object's maximum_segment_length says.
    pub fn start(
        controller: &mut Controller,
        now: Instant,
        entity_id: EntityId,
        object: &MemoryObjectDescriptor<'_>,
        image: Vec<u8>,
        restart: bool,
    ) -> Self {
        let segment = object
            .maximum_segment_length
            .filter(|&length| length > 0)
            .map_or(MOST_PER_WRITE, |length| {
                usize::try_from(length).map_or(MOST_PER_WRITE, |length| length.min(MOST_PER_WRITE))
            });
        // A maximum_length of 0 says nothing of the size.
        let fits = !image.is_empty()
            && (object.maximum_length == 0 || image.len() as u64 <= object.maximum_length);
        let mut upload = Self {
            entity_id,
            index: object.index,
            address: object.start_address,
            image,
            segment,
            restart,
            stage: Stage::Locking,
            pending: None,
            relock: None,
            unlock: None,
            locked: false,
            locked_at: now,
            upload_id: None,
            store_id: None,
            sent: 0,
            sends: 0,
            abort: false,
            deadline: None,
        };
        if fits {
            let command = controller.lock_entity(now, entity_id, false);
            upload.pending = Some((command, Asked::Lock));
        } else {
            upload.stage = Stage::Failed(Failure::DoesNotFit);
        }
        upload
    }

    pub fn entity_id(&self) -> EntityId {
        self.entity_id
    }

    /// The memory object's index.
    pub fn index(&self) -> u16 {
        self.index
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Whether it is done, stopped or failed.
    pub fn is_over(&self) -> bool {
        matches!(self.stage, Stage::Done | Stage::Aborted | Stage::Failed(_))
    }

    /// Stops the upload, unless storing has begun, which cannot be stopped:
    /// whether it will stop. It stops once the command in flight is
    /// answered, with ABORT_OPERATION once the entity has begun the upload.
    pub fn abort(&mut self, controller: &mut Controller, now: Instant) -> bool {
        match self.stage {
            Stage::Locking | Stage::Starting | Stage::Writing { .. } => {
                self.abort = true;
                self.stage = Stage::Aborting;
                if self.pending.is_none() {
                    self.send_abort(controller, now);
                }
                true
            }
            Stage::Aborting => true,
            _ => false,
        }
    }

    /// Takes a command the controller finished: whether it was the
    /// upload's.
    pub fn handle_finished(
        &mut self,
        controller: &mut Controller,
        now: Instant,
        command: CommandId,
        outcome: Outcome,
    ) -> bool {
        if self.relock == Some(command) {
            self.relock = None;
            return true;
        }
        if self.unlock == Some(command) {
            self.unlock = None;
            return true;
        }
        let Some((pending, asked)) = self.pending else {
            return false;
        };
        if pending != command {
            return false;
        }
        self.pending = None;
        let held = |status| {
            matches!(
                status,
                AemStatus::ENTITY_LOCKED | AemStatus::ENTITY_ACQUIRED
            )
        };
        match asked {
            Asked::Lock => match outcome {
                Outcome::Done => {
                    self.locked = true;
                    self.locked_at = now;
                    self.begin(controller, now);
                }
                // An entity without locks takes the upload as it is.
                Outcome::Refused(Refusal::Aem(
                    AemStatus::NOT_IMPLEMENTED | AemStatus::NOT_SUPPORTED,
                )) => self.begin(controller, now),
                Outcome::Refused(Refusal::Aem(status)) if held(status) => {
                    self.fail(controller, now, Failure::Locked);
                }
                outcome => self.fail(controller, now, Failure::Refused(Step::Lock, outcome)),
            },
            Asked::Upload => match outcome {
                Outcome::Done => {
                    self.upload_id = controller.take_operation_id(command);
                    if self.abort {
                        self.send_abort(controller, now);
                    } else {
                        self.write(controller, now);
                    }
                }
                Outcome::Refused(Refusal::Aem(status)) if held(status) => {
                    self.fail(controller, now, Failure::Locked);
                }
                outcome => self.fail(controller, now, Failure::Refused(Step::Upload, outcome)),
            },
            Asked::Write(length) => {
                let status = controller.take_memory_read(command).map(|read| read.status);
                match (outcome, status) {
                    (Outcome::Done, Some(AaStatus::SUCCESS)) => {
                        self.sent += length;
                        self.sends = 0;
                        if self.abort {
                            self.send_abort(controller, now);
                        } else if self.sent == self.image.len() {
                            self.store(controller, now);
                        } else {
                            self.write(controller, now);
                        }
                    }
                    (Outcome::Done, Some(status)) => {
                        let offset = self.sent as u64;
                        self.fail(controller, now, Failure::Write { offset, status });
                    }
                    (Outcome::NoResponse, _) if self.abort => self.send_abort(controller, now),
                    (Outcome::NoResponse, _) if self.sends < WRITE_SENDS => {
                        self.write(controller, now);
                    }
                    (outcome, _) => {
                        self.fail(controller, now, Failure::Refused(Step::Write, outcome));
                    }
                }
            }
            Asked::Store => match outcome {
                Outcome::Done => {
                    self.store_id = controller.take_operation_id(command);
                    self.stage = Stage::Storing { progress: None };
                    self.deadline = Some(now + STORE_TIME);
                }
                outcome => self.fail(controller, now, Failure::Refused(Step::Store, outcome)),
            },
            // Whatever the answer, nothing more goes to the entity.
            Asked::Abort => self.end(controller, now, Stage::Aborted),
        }
        true
    }

    /// Takes an OPERATION_STATUS report: whether it was the upload's.
    /// 1000 when storing ends it, 1-999 and 0xFFFF tell how far storing
    /// has got, and any other value ends it failed.
    pub fn handle_status(
        &mut self,
        controller: &mut Controller,
        now: Instant,
        status: &OperationStatus,
    ) -> bool {
        if self.is_over()
            || status.entity_id != self.entity_id
            || status.descriptor_type != DescriptorType::MEMORY_OBJECT
            || status.index != self.index
        {
            return false;
        }
        let storing = matches!(self.stage, Stage::Storing { .. })
            || matches!(self.pending, Some((_, Asked::Store)));
        // The store's own reports may come before its operation_id does.
        let ours = Some(status.operation_id) == self.upload_id
            || Some(status.operation_id) == self.store_id
            || (storing && self.store_id.is_none());
        if !ours {
            return false;
        }
        match status.percent_complete {
            1000 if storing => self.end(controller, now, Stage::Done),
            1000 => {}
            0xffff if storing => self.stage = Stage::Storing { progress: None },
            1..=999 if storing => {
                self.stage = Stage::Storing {
                    progress: Some(status.percent_complete),
                };
            }
            1..=999 | 0xffff => {}
            _ => self.fail(controller, now, Failure::Rejected),
        }
        true
    }

    /// Takes an entity going offline or restarting: whether it was the
    /// upload's. Going once it has begun storing with a restart asked is
    /// the restart, and the upload done.
    pub fn handle_gone(
        &mut self,
        controller: &mut Controller,
        now: Instant,
        entity_id: EntityId,
    ) -> bool {
        if entity_id != self.entity_id || self.is_over() {
            return false;
        }
        let stored = matches!(self.stage, Stage::Storing { .. }) && self.pending.is_none();
        self.locked = false;
        if self.restart && stored {
            self.end(controller, now, Stage::Done);
        } else {
            self.fail(controller, now, Failure::Gone);
        }
        true
    }

    /// When [`handle_timeout`](Self::handle_timeout) is due next.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.is_over() {
            return None;
        }
        let lock = self.locked.then(|| self.locked_at + LOCK_AGAIN);
        match (lock, self.deadline) {
            (Some(lock), Some(deadline)) => Some(lock.min(deadline)),
            (lock, deadline) => lock.or(deadline),
        }
    }

    /// Takes the lock again when due, and gives up on a store the entity
    /// has said nothing more of.
    pub fn handle_timeout(&mut self, controller: &mut Controller, now: Instant) {
        if self.is_over() {
            return;
        }
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.fail(controller, now, Failure::NoStatus);
            return;
        }
        if self.locked && self.locked_at + LOCK_AGAIN <= now {
            self.locked_at = now;
            if self.relock.is_none() {
                self.relock = Some(controller.lock_entity(now, self.entity_id, false));
            }
        }
    }

    /// Asks the entity to take the upload.
    fn begin(&mut self, controller: &mut Controller, now: Instant) {
        if self.abort {
            self.end(controller, now, Stage::Aborted);
            return;
        }
        let length = (self.image.len() as u64).to_be_bytes();
        let command = controller.start_operation(
            now,
            self.entity_id,
            DescriptorType::MEMORY_OBJECT,
            self.index,
            MemoryObjectOperation::UPLOAD.0,
            &length,
        );
        self.pending = Some((command, Asked::Upload));
        self.stage = Stage::Starting;
    }

    /// Sends the WRITE of the octets from `sent`.
    fn write(&mut self, controller: &mut Controller, now: Instant) {
        let end = (self.sent + self.segment).min(self.image.len());
        let command = controller.write_memory(
            now,
            self.entity_id,
            self.address + self.sent as u64,
            &self.image[self.sent..end],
        );
        self.sends += 1;
        self.pending = Some((command, Asked::Write(end - self.sent)));
        self.stage = Stage::Writing {
            sent: self.sent as u64,
            total: self.image.len() as u64,
        };
    }

    fn store(&mut self, controller: &mut Controller, now: Instant) {
        let operation = if self.restart {
            MemoryObjectOperation::STORE_AND_REBOOT
        } else {
            MemoryObjectOperation::STORE
        };
        let command = controller.start_operation(
            now,
            self.entity_id,
            DescriptorType::MEMORY_OBJECT,
            self.index,
            operation.0,
            &[],
        );
        self.pending = Some((command, Asked::Store));
        self.stage = Stage::Storing { progress: None };
    }

    fn send_abort(&mut self, controller: &mut Controller, now: Instant) {
        let Some(upload_id) = self.upload_id else {
            self.end(controller, now, Stage::Aborted);
            return;
        };
        let command = controller.abort_operation(
            now,
            self.entity_id,
            DescriptorType::MEMORY_OBJECT,
            self.index,
            upload_id,
        );
        self.pending = Some((command, Asked::Abort));
        self.stage = Stage::Aborting;
    }

    /// Ends it failed, or stopped where the caller asked to stop.
    fn fail(&mut self, controller: &mut Controller, now: Instant, failure: Failure) {
        let stage = if self.abort {
            Stage::Aborted
        } else {
            Stage::Failed(failure)
        };
        self.end(controller, now, stage);
    }

    /// Ends it as `stage` says, letting the lock go unless the entity
    /// restarts, which lets it go.
    fn end(&mut self, controller: &mut Controller, now: Instant, stage: Stage) {
        self.stage = stage;
        self.pending = None;
        self.deadline = None;
        let restarts = stage == Stage::Done && self.restart;
        if self.locked && !restarts {
            self.unlock = Some(controller.lock_entity(now, self.entity_id, true));
        }
        self.locked = false;
    }
}
