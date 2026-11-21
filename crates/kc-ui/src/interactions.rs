//! Using apps: sending interactions (commands, buttons and menus, forms)
//! and following Discord's answers on the gateway, matched by nonce.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use kc_core::Snowflake;
use kc_http::InteractionPlace;
use serde_json::Value;

use crate::session::AccountId;
use crate::shell::AppState;

/// An app that has not answered by then is reported as not responding.
const ANSWER_WAIT: Duration = Duration::from_secs(15);

/// What happened to an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The app answered (or opened a form).
    Done,
    /// It failed, with a reason to show.
    Failed(String),
}

/// An app's suggestions: (shown name, value) pairs.
pub(crate) type Choices = Vec<(String, Value)>;

/// What to do when an interaction's answer comes.
pub(crate) enum Waiting {
    /// Told when the interaction succeeds or fails.
    Outcome(Box<dyn FnOnce(Outcome)>),
    /// Given the app's suggestions for the option being typed.
    Suggestions(Box<dyn FnOnce(Choices)>),
}

struct Pending {
    waiting: Waiting,
    sent: Instant,
}

thread_local! {
    /// Interactions sent and not yet answered, by nonce.
    static PENDING: RefCell<HashMap<String, Pending>> = RefCell::new(HashMap::new());
}

impl AppState {
    /// Where an interaction with `application_id` in the open channel takes
    /// place, and the account's client to send it with.
    pub(crate) fn interaction_place(
        &self,
        application_id: Snowflake,
    ) -> Option<(kc_http::HttpClient, InteractionPlace)> {
        let (account, http, _) = self.active()?;
        let session_id = crate::session::session_id(account)?;
        let (channel_id, guild_id) = self
            .with_session(|session| {
                let channel = session.selected_channel?;
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                Some((channel, guild))
            })
            .flatten()?;
        Some((
            http,
            InteractionPlace {
                application_id,
                guild_id,
                channel_id,
                session_id,
            },
        ))
    }

    /// Sends an interaction `body` (built from an [`InteractionPlace`]
    /// with `nonce`), then tells `waiting` how it went.
    pub(crate) fn send_interaction(
        self: &Rc<Self>,
        http: kc_http::HttpClient,
        body: Value,
        nonce: String,
        waiting: Waiting,
    ) {
        PENDING.with_borrow_mut(|pending| {
            pending.insert(
                nonce.clone(),
                Pending {
                    waiting,
                    sent: Instant::now(),
                },
            );
        });
        let autocomplete = body["type"] == 4;
        let failed_nonce = nonce.clone();
        crate::video_setup::spawn_local(
            self,
            async move { http.interact(&body).await },
            move |app, result| {
                if let Err(error) = result {
                    tracing::warn!(%error, "an interaction was refused");
                    app.finish_interaction(&failed_nonce, Outcome::Failed(error.to_string()));
                }
            },
        );
        // Answers (or the lack of one) arrive on the gateway.
        if !autocomplete {
            let app = self.clone();
            glib::timeout_add_local_once(ANSWER_WAIT, move || {
                let late = PENDING.with_borrow(|pending| {
                    pending
                        .get(&nonce)
                        .is_some_and(|pending| pending.sent.elapsed() >= ANSWER_WAIT)
                });
                if late {
                    app.finish_interaction(
                        &nonce,
                        Outcome::Failed("The application did not respond".to_owned()),
                    );
                }
            });
        }
    }

    fn finish_interaction(self: &Rc<Self>, nonce: &str, outcome: Outcome) {
        let Some(pending) = PENDING.with_borrow_mut(|pending| pending.remove(nonce)) else {
            return;
        };
        match pending.waiting {
            Waiting::Outcome(done) => {
                if let Outcome::Failed(reason) = &outcome {
                    self.set_status(reason);
                }
                done(outcome);
            }
            Waiting::Suggestions(done) => done(Vec::new()),
        }
    }

    /// Discord's answer to an interaction, from the gateway.
    pub(crate) fn on_interaction_event(self: &Rc<Self>, account: AccountId, name: &str, data: &Value) {
        // Only the account in view sends interactions.
        if self.active().map(|(active, ..)| active) != Some(account) {
            return;
        }
        let nonce = data["nonce"].as_str().unwrap_or_default();
        match name {
            "INTERACTION_SUCCESS" => self.finish_interaction(nonce, Outcome::Done),
            "INTERACTION_FAILURE" => self.finish_interaction(
                nonce,
                Outcome::Failed("The application did not respond".to_owned()),
            ),
            "INTERACTION_MODAL_CREATE" => {
                self.finish_interaction(nonce, Outcome::Done);
                self.show_app_form(data);
            }
            "INTERACTION_IFRAME_MODAL_CREATE" => {
                self.finish_interaction(nonce, Outcome::Done);
                self.set_status("This app opened a web view, which Kubecord cannot show");
            }
            "APPLICATION_COMMAND_AUTOCOMPLETE_RESPONSE" => {
                let Some(pending) = PENDING.with_borrow_mut(|pending| pending.remove(nonce)) else {
                    return;
                };
                let choices = data["choices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|choice| {
                        Some((choice["name"].as_str()?.to_owned(), choice["value"].clone()))
                    })
                    .collect();
                if let Waiting::Suggestions(done) = pending.waiting {
                    done(choices);
                }
            }
            _ => {}
        }
    }
}
