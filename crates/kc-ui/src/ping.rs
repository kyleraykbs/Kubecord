//! The call's ping: signal bars in the voice panel, and a graph of the last
//! few minutes when they are clicked, as in Discord.

use std::rc::Rc;

use gtk::prelude::*;

use crate::shell::AppState;

/// Samples kept: five minutes at one every five seconds.
const HISTORY: usize = 60;

/// One measurement: when (Unix seconds), and the round trip in milliseconds,
/// or `None` if the reply never came.
pub(crate) type Sample = (i64, Option<u32>);

/// How good a ping is, as bars lit out of four.
fn bars(millis: Option<u32>) -> u32 {
    match millis {
        None => 0,
        Some(0..=99) => 4,
        Some(100..=199) => 3,
        Some(200..=349) => 2,
        Some(_) => 1,
    }
}

/// Green, yellow or red for a number of bars.
fn bar_color(lit: u32) -> (f64, f64, f64) {
    match lit {
        3.. => (0.26, 0.71, 0.42),
        2 => (0.94, 0.70, 0.20),
        _ => (0.93, 0.26, 0.27),
    }
}

/// The signal bars and the popover's widgets.
pub(crate) struct PingWidgets {
    pub button: gtk::MenuButton,
    icon: gtk::DrawingArea,
    graph: gtk::DrawingArea,
    last: gtk::Label,
    average: gtk::Label,
    lost: gtk::Label,
}

impl PingWidgets {
    pub(crate) fn new() -> Self {
        let icon = gtk::DrawingArea::new();
        icon.set_content_width(18);
        icon.set_content_height(16);
        let button = gtk::MenuButton::new();
        button.add_css_class("flat");
        button.add_css_class("circular");
        button.set_valign(gtk::Align::Center);
        button.set_child(Some(&icon));
        button.set_tooltip_text(Some("Connection"));
        button.set_direction(gtk::ArrowType::Up);

        let graph = gtk::DrawingArea::new();
        graph.set_content_width(340);
        graph.set_content_height(140);
        let title = gtk::Label::new(Some("Voice Connection"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        let line = |text: &str| {
            let label = gtk::Label::new(Some(text));
            label.set_xalign(0.0);
            label
        };
        let last = line("Last ping: …");
        let average = line("Average ping: …");
        let lost = line("");
        lost.add_css_class("dim-label");
        lost.add_css_class("caption");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&title);
        content.append(&graph);
        content.append(&average);
        content.append(&last);
        content.append(&lost);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&content));
        button.set_popover(Some(&popover));
        Self {
            button,
            icon,
            graph,
            last,
            average,
            lost,
        }
    }
}

impl AppState {
    /// Sets up drawing; the widgets read the history when they redraw.
    pub(crate) fn connect_ping(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.ping
            .icon
            .set_draw_func(move |area, cairo, width, height| {
                let Some(app) = app.upgrade() else {
                    return;
                };
                let latest = app.ping_history.borrow().back().map(|(_, millis)| *millis);
                let lit = latest.map(bars);
                let ink = area.color();
                let (red, green, blue) = bar_color(lit.unwrap_or(0));
                let bar_width = f64::from(width) / 4.0 - 1.5;
                for bar in 0..4u32 {
                    let bar_height = f64::from(height) * f64::from(bar + 1) / 4.0;
                    let x = f64::from(bar) * (bar_width + 1.5);
                    if lit.is_some_and(|lit| bar < lit.max(1)) {
                        cairo.set_source_rgb(red, green, blue);
                    } else {
                        cairo.set_source_rgba(
                            f64::from(ink.red()),
                            f64::from(ink.green()),
                            f64::from(ink.blue()),
                            0.25,
                        );
                    }
                    cairo.rectangle(x, f64::from(height) - bar_height, bar_width, bar_height);
                    let _ = cairo.fill();
                }
            });
        let app = Rc::downgrade(self);
        self.ping
            .graph
            .set_draw_func(move |area, cairo, width, height| {
                if let Some(app) = app.upgrade() {
                    draw_graph(area, cairo, width, height, &app.ping_history.borrow());
                }
            });
    }

    /// Records a measurement and shows it.
    pub(crate) fn note_ping(&self, millis: Option<u32>) {
        let now = gtk::glib::DateTime::now_local()
            .map(|now| now.to_unix())
            .unwrap_or(0);
        {
            let mut history = self.ping_history.borrow_mut();
            history.push_back((now, millis));
            while history.len() > HISTORY {
                history.pop_front();
            }
        }
        self.render_ping();
    }

    /// Forgets the history when a call ends.
    pub(crate) fn clear_ping(&self) {
        self.ping_history.borrow_mut().clear();
        self.render_ping();
    }

    fn render_ping(&self) {
        let history = self.ping_history.borrow();
        let replies: Vec<u32> = history.iter().filter_map(|(_, millis)| *millis).collect();
        let lost = history.len() - replies.len();
        self.ping.last.set_label(&match history.back() {
            Some((_, Some(millis))) => format!("Last ping: {millis} ms"),
            Some((_, None)) => "Last ping: no reply".to_owned(),
            None => "Last ping: …".to_owned(),
        });
        self.ping.average.set_label(&if replies.is_empty() {
            "Average ping: …".to_owned()
        } else {
            format!(
                "Average ping: {} ms",
                replies.iter().map(|millis| u64::from(*millis)).sum::<u64>() / replies.len() as u64
            )
        });
        self.ping.lost.set_label(&if lost == 0 {
            "Measured every 5 seconds. No pings lost.".to_owned()
        } else {
            format!(
                "Measured every 5 seconds. {lost} of the last {} lost.",
                history.len()
            )
        });
        self.ping
            .button
            .set_tooltip_text(Some(&match history.back() {
                Some((_, Some(millis))) => format!("Ping: {millis} ms"),
                Some((_, None)) => "Ping: no reply".to_owned(),
                None => "Connection".to_owned(),
            }));
        self.ping.icon.queue_draw();
        self.ping.graph.queue_draw();
    }
}

/// The top of the graph's scale for the largest ping shown: at least 100,
/// in steps of 50.
fn scale_top(largest: u32) -> u32 {
    largest.max(1).div_ceil(50).max(2) * 50
}

fn draw_graph(
    area: &gtk::DrawingArea,
    cairo: &gtk::cairo::Context,
    width: i32,
    height: i32,
    history: &std::collections::VecDeque<Sample>,
) {
    let ink = area.color();
    let ink = |alpha: f64| {
        cairo.set_source_rgba(
            f64::from(ink.red()),
            f64::from(ink.green()),
            f64::from(ink.blue()),
            alpha,
        );
    };
    let (width, height) = (f64::from(width), f64::from(height));
    // Room for the scale on the right and the times underneath.
    let (plot_width, plot_height) = (width - 34.0, height - 18.0);
    let top = scale_top(
        history
            .iter()
            .filter_map(|(_, millis)| *millis)
            .max()
            .unwrap_or(0),
    );
    cairo.set_font_size(10.0);
    cairo.set_line_width(1.0);
    for step in 0..=2u32 {
        let value = top * step / 2;
        let y = (plot_height - plot_height * f64::from(step) / 2.0).round() + 0.5;
        ink(0.15);
        cairo.move_to(0.0, y);
        cairo.line_to(plot_width, y);
        let _ = cairo.stroke();
        ink(0.6);
        cairo.move_to(plot_width + 6.0, (y + 3.5).min(plot_height + 3.5).max(9.0));
        let _ = cairo.show_text(&value.to_string());
    }
    let Some((&(first, _), &(last, _))) = history.front().zip(history.back()) else {
        return;
    };
    // The window always spans the full history length, so the line grows
    // in from the right as in Discord.
    let span = (HISTORY as i64 * 5).max(last - first).max(1) as f64;
    let x_of = |at: i64| plot_width - (last - at) as f64 / span * plot_width;
    let y_of = |millis: u32| {
        plot_height - (f64::from(millis.min(top)) / f64::from(top)) * (plot_height - 2.0) - 1.0
    };

    // A label at each whole minute.
    ink(0.6);
    let mut minute = last - last.rem_euclid(60);
    while x_of(minute) >= 0.0 {
        if let Ok(time) = gtk::glib::DateTime::from_unix_local(minute)
            && let Ok(text) = time.format("%-I:%M %p")
        {
            let x = x_of(minute);
            if x >= 24.0 && x <= plot_width - 24.0 {
                cairo.move_to(x - 22.0, height - 4.0);
                let _ = cairo.show_text(&text);
            }
        }
        minute -= 60;
    }

    // The line, broken where a ping was lost; a red mark stands for the loss.
    cairo.set_source_rgb(0.42, 0.51, 0.95);
    cairo.set_line_width(1.5);
    let mut drawing = false;
    for (at, millis) in history {
        match millis {
            Some(millis) => {
                let (x, y) = (x_of(*at), y_of(*millis));
                if drawing {
                    cairo.line_to(x, y);
                } else {
                    cairo.move_to(x, y);
                    // A lone sample still shows, as a dot.
                    cairo.arc(x, y, 0.8, 0.0, std::f64::consts::TAU);
                    cairo.move_to(x, y);
                    drawing = true;
                }
            }
            None => drawing = false,
        }
    }
    let _ = cairo.stroke();
    cairo.set_source_rgb(0.93, 0.26, 0.27);
    for (at, millis) in history {
        if millis.is_none() {
            cairo.rectangle(x_of(*at) - 1.0, 0.0, 2.0, plot_height);
        }
    }
    let _ = cairo.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_and_scale_follow_the_ping() {
        assert_eq!(bars(Some(23)), 4);
        assert_eq!(bars(Some(150)), 3);
        assert_eq!(bars(Some(250)), 2);
        assert_eq!(bars(Some(900)), 1);
        assert_eq!(bars(None), 0);
        assert_eq!(scale_top(0), 100);
        assert_eq!(scale_top(48), 100);
        assert_eq!(scale_top(101), 150);
        assert_eq!(scale_top(420), 450);
    }
}
