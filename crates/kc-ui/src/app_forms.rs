//! Forms apps open (Discord's "modals"): text fields, select menus and
//! text, filled in and submitted back to the app.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use kc_core::Snowflake;
use libadwaita as adw;
use libadwaita::prelude::*;
use serde_json::{Value, json};

use crate::interactions::{Outcome, Waiting};
use crate::shell::AppState;

/// One field of a form, and how to read what was put in it.
enum Field {
    Text {
        custom_id: String,
        read: Box<dyn Fn() -> String>,
        required: bool,
        min: usize,
        max: usize,
        /// Inside a Label (newer forms) rather than an action row.
        labelled: bool,
    },
    Select {
        custom_id: String,
        read: Box<dyn Fn() -> Vec<String>>,
        required: bool,
        min: usize,
    },
}

impl Field {
    fn is_filled(&self) -> bool {
        match self {
            Field::Text {
                read,
                required,
                min,
                max,
                ..
            } => {
                let length = read().chars().count();
                (length > 0 || !required) && (length == 0 || (length >= *min && length <= *max))
            }
            Field::Select { read, required, min, .. } => {
                let chosen = read().len();
                !(*required && chosen < (*min).max(1))
            }
        }
    }

    /// What the app gets back for this field.
    fn submitted(&self) -> Value {
        match self {
            Field::Text {
                custom_id,
                read,
                labelled,
                ..
            } => {
                let input = json!({"type": 4, "custom_id": custom_id, "value": read()});
                if *labelled {
                    json!({"type": 18, "component": input})
                } else {
                    json!({"type": 1, "components": [input]})
                }
            }
            Field::Select { custom_id, read, .. } => {
                json!({"type": 18, "component": {"type": 3, "custom_id": custom_id, "values": read()}})
            }
        }
    }
}

/// A heading for a field: its label (with a mark when required) and any
/// description.
fn field_heading(text: &str, description: Option<&str>, required: bool) -> gtk::Box {
    let heading = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title = gtk::Label::new(Some(&if required {
        format!("{text} *")
    } else {
        text.to_owned()
    }));
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.add_css_class("heading");
    heading.append(&title);
    if let Some(description) = description.filter(|text| !text.is_empty()) {
        let note = gtk::Label::new(Some(description));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        heading.append(&note);
    }
    heading
}

/// A text field (`style` 1 is one line, 2 a paragraph). Calls `changed` on
/// every edit.
fn text_input(input: &Value, changed: Rc<dyn Fn()>) -> (gtk::Widget, Box<dyn Fn() -> String>) {
    let value = input["value"].as_str().unwrap_or_default();
    let placeholder = input["placeholder"].as_str().unwrap_or_default();
    let max = input["max_length"].as_u64().unwrap_or(4000) as i32;
    if input["style"].as_u64() == Some(2) {
        let view = gtk::TextView::new();
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_accepts_tab(false);
        view.set_top_margin(8);
        view.set_bottom_margin(8);
        view.set_left_margin(8);
        view.set_right_margin(8);
        view.buffer().set_text(value);
        view.set_tooltip_text((!placeholder.is_empty()).then_some(placeholder));
        view.buffer().connect_changed(move |_| changed());
        let frame = gtk::ScrolledWindow::new();
        frame.set_child(Some(&view));
        frame.set_min_content_height(96);
        frame.set_max_content_height(200);
        frame.set_propagate_natural_height(true);
        frame.add_css_class("card");
        let buffer = view.buffer();
        let read = Box::new(move || {
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string()
        });
        (frame.upcast(), read)
    } else {
        let entry = gtk::Entry::new();
        entry.set_text(value);
        entry.set_placeholder_text((!placeholder.is_empty()).then_some(placeholder));
        entry.set_max_length(max);
        entry.connect_changed(move |_| changed());
        let reader = entry.clone();
        (entry.upcast(), Box::new(move || reader.text().to_string()))
    }
}

/// A select menu: a drop-down for one choice, check boxes for several.
fn select_input(select: &Value, changed: Rc<dyn Fn()>) -> (gtk::Widget, Box<dyn Fn() -> Vec<String>>) {
    let options: Vec<(String, String, bool)> = select["options"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|option| {
            (
                option["label"].as_str().unwrap_or_default().to_owned(),
                option["value"].as_str().unwrap_or_default().to_owned(),
                option["default"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    let max = select["max_values"].as_u64().unwrap_or(1) as usize;
    if max <= 1 {
        let mut labels: Vec<&str> = vec![select["placeholder"].as_str().unwrap_or("Make a selection")];
        labels.extend(options.iter().map(|(label, ..)| label.as_str()));
        let dropdown = gtk::DropDown::from_strings(&labels);
        if let Some(index) = options.iter().position(|(.., default)| *default) {
            dropdown.set_selected(index as u32 + 1);
        }
        dropdown.connect_selected_notify(move |_| changed());
        let values: Vec<String> = options.iter().map(|(_, value, _)| value.clone()).collect();
        let reader = dropdown.clone();
        let read = Box::new(move || {
            let selected = reader.selected() as usize;
            values
                .get(selected.wrapping_sub(1))
                .filter(|_| selected > 0)
                .cloned()
                .into_iter()
                .collect()
        });
        (dropdown.upcast(), read)
    } else {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let mut checks = Vec::new();
        for (label, value, default) in &options {
            let check = gtk::CheckButton::with_label(label);
            check.set_active(*default);
            let changed = changed.clone();
            check.connect_toggled(move |_| changed());
            list.append(&check);
            checks.push((check, value.clone()));
        }
        let read = Box::new(move || {
            checks
                .iter()
                .filter(|(check, _)| check.is_active())
                .map(|(_, value)| value.clone())
                .collect()
        });
        (list.upcast(), read)
    }
}

impl AppState {
    /// Shows a form an app opened (`INTERACTION_MODAL_CREATE`), and sends
    /// what is filled in when it is submitted.
    pub(crate) fn show_app_form(self: &Rc<Self>, data: &Value) {
        let Some(modal_id) = data["id"].as_str().and_then(|id| id.parse::<Snowflake>().ok()) else {
            return;
        };
        let Some(application_id) = data["application"]["id"]
            .as_str()
            .and_then(|id| id.parse::<Snowflake>().ok())
        else {
            return;
        };
        let custom_id = data["custom_id"].as_str().unwrap_or_default().to_owned();
        let title = data["title"].as_str().unwrap_or("Form");
        let app_name = data["application"]["name"].as_str().unwrap_or_default();

        let dialog = adw::AlertDialog::new(Some(title), (!app_name.is_empty()).then_some(app_name));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("submit", "Submit");
        dialog.set_response_appearance("submit", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("submit"));
        dialog.set_close_response("cancel");

        let fields: Rc<RefCell<Vec<Field>>> = Rc::default();
        let check = {
            let fields = fields.clone();
            let dialog = dialog.downgrade();
            Rc::new(move || {
                if let (Some(dialog), Ok(fields)) = (dialog.upgrade(), fields.try_borrow()) {
                    dialog.set_response_enabled("submit", fields.iter().all(Field::is_filled));
                }
            }) as Rc<dyn Fn()>
        };

        let form = gtk::Box::new(gtk::Orientation::Vertical, 14);
        form.set_width_request(360);
        for component in data["components"].as_array().into_iter().flatten() {
            match component["type"].as_u64() {
                // An action row holding one text input (the older forms).
                Some(1) => {
                    for input in component["components"].as_array().into_iter().flatten() {
                        if input["type"].as_u64() != Some(4) {
                            continue;
                        }
                        let required = input["required"].as_bool().unwrap_or(true);
                        let field = gtk::Box::new(gtk::Orientation::Vertical, 6);
                        field.append(&field_heading(input["label"].as_str().unwrap_or_default(), None, required));
                        let (widget, read) = text_input(input, check.clone());
                        field.append(&widget);
                        form.append(&field);
                        fields.borrow_mut().push(Field::Text {
                            custom_id: input["custom_id"].as_str().unwrap_or_default().to_owned(),
                            read,
                            required,
                            min: input["min_length"].as_u64().unwrap_or(0) as usize,
                            max: input["max_length"].as_u64().unwrap_or(4000) as usize,
                            labelled: false,
                        });
                    }
                }
                // Text shown in the form.
                Some(10) => {
                    let text = gtk::Label::new(Some(component["content"].as_str().unwrap_or_default()));
                    text.set_xalign(0.0);
                    text.set_wrap(true);
                    form.append(&text);
                }
                // A labelled field (the newer forms).
                Some(18) => {
                    let inner = &component["component"];
                    let required = inner["required"].as_bool().unwrap_or(true);
                    let field = gtk::Box::new(gtk::Orientation::Vertical, 6);
                    field.append(&field_heading(
                        component["label"].as_str().unwrap_or_default(),
                        component["description"].as_str(),
                        required,
                    ));
                    let inner_id = inner["custom_id"].as_str().unwrap_or_default().to_owned();
                    match inner["type"].as_u64() {
                        Some(4) => {
                            let (widget, read) = text_input(inner, check.clone());
                            field.append(&widget);
                            fields.borrow_mut().push(Field::Text {
                                custom_id: inner_id,
                                read,
                                required,
                                min: inner["min_length"].as_u64().unwrap_or(0) as usize,
                                max: inner["max_length"].as_u64().unwrap_or(4000) as usize,
                                labelled: true,
                            });
                        }
                        Some(3) => {
                            let (widget, read) = select_input(inner, check.clone());
                            field.append(&widget);
                            fields.borrow_mut().push(Field::Select {
                                custom_id: inner_id,
                                read,
                                required,
                                min: inner["min_values"].as_u64().unwrap_or(1) as usize,
                            });
                        }
                        _ => {
                            let note = gtk::Label::new(Some("This field is not supported in Kubecord yet."));
                            note.set_xalign(0.0);
                            note.add_css_class("dim-label");
                            field.append(&note);
                        }
                    }
                    form.append(&field);
                }
                _ => {}
            }
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&form));
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(480);
        dialog.set_extra_child(Some(&scroll));
        check();

        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "submit" {
                return;
            }
            let components: Vec<Value> = fields.borrow().iter().map(Field::submitted).collect();
            let Some((http, place)) = app.interaction_place(application_id) else {
                app.set_status("Not connected yet: the form was not sent");
                return;
            };
            let nonce = kc_http::HttpClient::new_nonce();
            let body = place.modal(modal_id, &custom_id, components, &nonce);
            app.send_interaction(
                http,
                body,
                nonce,
                Waiting::Outcome(Box::new(|outcome| {
                    if let Outcome::Failed(reason) = outcome {
                        tracing::info!(reason, "a form was not accepted");
                    }
                })),
            );
        });
        dialog.present(Some(&self.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_report_what_the_app_expects() {
        let text = Field::Text {
            custom_id: "name".to_owned(),
            read: Box::new(|| "Ada".to_owned()),
            required: true,
            min: 2,
            max: 10,
            labelled: false,
        };
        assert!(text.is_filled());
        assert_eq!(
            text.submitted(),
            json!({"type": 1, "components": [{"type": 4, "custom_id": "name", "value": "Ada"}]})
        );
        let short = Field::Text {
            custom_id: "x".to_owned(),
            read: Box::new(|| "A".to_owned()),
            required: false,
            min: 2,
            max: 10,
            labelled: true,
        };
        assert!(!short.is_filled());
        let select = Field::Select {
            custom_id: "pick".to_owned(),
            read: Box::new(Vec::new),
            required: true,
            min: 1,
        };
        assert!(!select.is_filled());
    }
}
