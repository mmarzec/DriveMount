// SPDX-License-Identifier: GPL-3.0-or-later

//! The prompts of spec §7.4, and the [`Prompter`] that backends ask them through.
//!
//! [`UiPrompter`] can be called from any thread, and only posts to a [`Mailbox`]. On the UI thread,
//! [`Prompts`] keeps what has come in as a queue, of which one shows at a time. That is what
//! serialises them: profiles mounting together all ask through the one prompter (FR-MNT-08). Dropping
//! a call's future withdraws its prompt, so that Cancel on a row does not leave a dialog behind.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fsm_core::profile::Problem;
use fsm_vfs::{
    ChangedHostKey, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    Prompter, Remember, Secret, SecretRequest, async_trait,
};
use tokio::sync::oneshot;

/// A prompt, with how to answer it.
pub(super) enum Question {
    Secret {
        request: SecretRequest,
        reply: oneshot::Sender<Option<(Secret<String>, Remember)>>,
    },
    Login {
        request: LoginRequest,
        reply: oneshot::Sender<Option<LoginAnswer>>,
    },
    /// One dialog for each of a round's questions, with nothing from another profile between them.
    KeyboardInteractive {
        request: KbdIntRequest,
        answers: Vec<Secret<String>>,
        reply: oneshot::Sender<Option<Vec<Secret<String>>>>,
    },
    HostKey {
        request: HostKeyRequest,
        reply: oneshot::Sender<HostKeyDecision>,
    },
    /// Nothing waits for the answer: the connection has been refused (FR-SSH-10).
    ChangedKey(Box<ChangedHostKey>),
}

/// What reaches the UI thread from others.
pub(super) enum Posted {
    Ask {
        ticket: u64,
        question: Question,
    },
    Withdraw {
        ticket: u64,
    },
    /// A key file has been looked at.
    KeyChecked {
        path: PathBuf,
        result: Result<(), Problem>,
    },
}

#[derive(Default)]
struct Letters {
    queue: VecDeque<Posted>,
    closed: bool,
}

/// How other threads reach the UI thread. The window cannot cross threads, but this can.
pub(super) struct Mailbox {
    letters: Mutex<Letters>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Mailbox {
    /// `wake` has to make the UI thread call [`take`](Self::take).
    pub(super) fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            letters: Mutex::default(),
            wake: Box::new(wake),
        }
    }

    fn letters(&self) -> MutexGuard<'_, Letters> {
        self.letters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Once closed, what is posted is dropped, which answers a prompt with a cancel.
    pub(super) fn post(&self, posted: Posted) {
        {
            let mut letters = self.letters();
            if letters.closed {
                return;
            }
            letters.queue.push_back(posted);
        }
        (self.wake)();
    }

    pub(super) fn take(&self) -> Vec<Posted> {
        self.letters().queue.drain(..).collect()
    }

    /// The window is gone: nobody will answer what is waiting, or what comes.
    pub(super) fn close(&self) {
        let mut letters = self.letters();
        letters.closed = true;
        letters.queue.clear();
    }
}

/// Withdraws a prompt if the call asking it is dropped before the answer comes.
struct Asking<'a> {
    mailbox: &'a Mailbox,
    ticket: Option<u64>,
}

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket {
            self.mailbox.post(Posted::Withdraw { ticket });
        }
    }
}

/// The [`Prompter`] the engine is given. It holds no window, so it can go anywhere.
pub struct UiPrompter {
    mailbox: Arc<Mailbox>,
    next_ticket: AtomicU64,
}

impl UiPrompter {
    pub(super) const fn new(mailbox: Arc<Mailbox>) -> Self {
        Self {
            mailbox,
            next_ticket: AtomicU64::new(1),
        }
    }

    /// Asks, and waits. `None` if the window went away without answering.
    async fn ask<T>(&self, question: impl FnOnce(oneshot::Sender<T>) -> Question) -> Option<T> {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        let mut asking = Asking {
            mailbox: &self.mailbox,
            ticket: Some(ticket),
        };
        self.mailbox.post(Posted::Ask {
            ticket,
            question: question(reply),
        });
        let answer = answer.await.ok();
        asking.ticket = None;
        answer
    }
}

#[async_trait]
impl Prompter for UiPrompter {
    async fn secret(&self, request: SecretRequest) -> Option<(Secret<String>, Remember)> {
        self.ask(|reply| Question::Secret { request, reply })
            .await
            .flatten()
    }

    async fn login(&self, request: LoginRequest) -> Option<LoginAnswer> {
        self.ask(|reply| Question::Login { request, reply })
            .await
            .flatten()
    }

    async fn keyboard_interactive(&self, request: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        // A round can hold no questions, only something to read. There is nothing to answer.
        if request.prompts.is_empty() {
            return Some(Vec::new());
        }
        self.ask(|reply| Question::KeyboardInteractive {
            request,
            answers: Vec::new(),
            reply,
        })
        .await
        .flatten()
    }

    async fn host_key(&self, request: HostKeyRequest) -> HostKeyDecision {
        self.ask(|reply| Question::HostKey { request, reply })
            .await
            .unwrap_or(HostKeyDecision::Reject)
    }
}

/// One entry of the queue. The window's own notices have no ticket, as nothing can withdraw them.
pub(super) struct Entry {
    pub(super) ticket: Option<u64>,
    pub(super) question: Question,
}

/// The queue, on the UI thread.
#[derive(Default)]
pub(super) struct Prompts {
    current: Option<Entry>,
    waiting: VecDeque<Entry>,
}

impl Prompts {
    pub(super) fn push(&mut self, entry: Entry) {
        self.waiting.push_back(entry);
    }

    /// Removes a prompt from the queue. Returns whether it was the one showing.
    pub(super) fn withdraw(&mut self, ticket: u64) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|entry| entry.ticket == Some(ticket))
        {
            self.current = None;
            return true;
        }
        self.waiting.retain(|entry| entry.ticket != Some(ticket));
        false
    }

    /// The next to show, if none is showing.
    pub(super) fn advance(&mut self) -> bool {
        if self.current.is_some() {
            return false;
        }
        self.current = self.waiting.pop_front();
        self.current.is_some()
    }

    pub(super) const fn current(&self) -> Option<&Entry> {
        self.current.as_ref()
    }

    pub(super) const fn current_mut(&mut self) -> Option<&mut Entry> {
        self.current.as_mut()
    }

    pub(super) const fn take_current(&mut self) -> Option<Entry> {
        self.current.take()
    }
}

/// What the server said to read before a round's questions.
pub(super) fn round_message(request: &KbdIntRequest) -> String {
    [request.name.trim(), request.instructions.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use fsm_vfs::KbdIntPrompt;

    use super::*;

    fn round(name: &str, instructions: &str) -> KbdIntRequest {
        KbdIntRequest {
            user: "me".to_owned(),
            host: "h".to_owned(),
            name: name.to_owned(),
            instructions: instructions.to_owned(),
            prompts: vec![KbdIntPrompt {
                text: "Code:".to_owned(),
                echo: false,
            }],
        }
    }

    #[test]
    fn a_rounds_message_is_what_the_server_said_without_blank_parts() {
        assert_eq!(round_message(&round("", "")), "");
        assert_eq!(round_message(&round(" 2FA ", "")), "2FA");
        assert_eq!(round_message(&round("", "Read this\n")), "Read this");
        assert_eq!(round_message(&round("2FA", "Read this")), "2FA\nRead this");
    }

    fn notice(ticket: Option<u64>) -> Entry {
        Entry {
            ticket,
            question: Question::ChangedKey(Box::new(ChangedHostKey {
                host: "h".to_owned(),
                port: 22,
                old_fingerprint: String::new(),
                new_fingerprint: String::new(),
                known_hosts: PathBuf::new(),
                line: 1,
            })),
        }
    }

    #[test]
    fn the_queue_shows_one_at_a_time_in_the_order_they_came() {
        let mut prompts = Prompts::default();
        prompts.push(notice(Some(1)));
        prompts.push(notice(Some(2)));
        assert!(prompts.advance());
        assert!(!prompts.advance(), "one is showing");
        assert_eq!(prompts.current().unwrap().ticket, Some(1));
        assert!(prompts.take_current().is_some());
        assert!(prompts.advance());
        assert_eq!(prompts.current().unwrap().ticket, Some(2));
        assert!(prompts.take_current().is_some());
        assert!(!prompts.advance(), "nothing is left");
    }

    #[test]
    fn withdrawing_says_whether_it_was_showing_and_leaves_the_rest() {
        let mut prompts = Prompts::default();
        prompts.push(notice(Some(1)));
        prompts.push(notice(Some(2)));
        prompts.push(notice(None));
        prompts.push(notice(Some(3)));
        prompts.advance();

        assert!(!prompts.withdraw(2), "it was waiting");
        assert!(!prompts.withdraw(99), "it has been answered");
        assert_eq!(prompts.current().unwrap().ticket, Some(1));
        assert!(prompts.withdraw(1));
        assert!(prompts.current().is_none());

        prompts.advance();
        assert_eq!(prompts.current().unwrap().ticket, None);
        prompts.take_current();
        prompts.advance();
        assert_eq!(prompts.current().unwrap().ticket, Some(3));
    }

    #[test]
    fn a_closed_mailbox_drops_what_is_in_it_and_what_comes_after() {
        let mailbox = Mailbox::new(|| {});
        let (first, first_answer) = oneshot::channel();
        let (second, second_answer) = oneshot::channel();
        let ask = |reply| Posted::Ask {
            ticket: 1,
            question: Question::HostKey {
                request: HostKeyRequest {
                    host: String::new(),
                    port: 22,
                    key_type: String::new(),
                    fingerprint: String::new(),
                },
                reply,
            },
        };
        mailbox.post(ask(first));
        mailbox.close();
        mailbox.post(ask(second));
        assert!(mailbox.take().is_empty());
        assert!(first_answer.blocking_recv().is_err());
        assert!(second_answer.blocking_recv().is_err());
    }
}
