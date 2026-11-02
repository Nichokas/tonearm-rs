//! Listen Together panel: a popover at the top of the queue view.
use adw::prelude::*;
use gtk::gdk;

use crate::i18n::tr;
use crate::live::{self, Phase};
use crate::ui::components::linking::render_qr;
use crate::ui::dsl::{W, label, on_state, spacing, vstack, hstack};

const QR_SIZE: i32 = 200;

fn wrapped(text: &str) -> gtk::Label {
    let l = label(text).halign(gtk::Align::Center);
    l.set_wrap(true);
    l.set_justify(gtk::Justification::Center);
    l.set_max_width_chars(32);
    l
}

fn title(text: &str) -> gtk::Label {
    label(text).class("title-4").halign(gtk::Align::Center)
}

fn button(text: &str, class: &str, f: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::with_label(text).class(class).class("pill");
    b.connect_clicked(move |_| f());
    b
}

fn output_delay_row() -> adw::SpinRow {
    let row = adw::SpinRow::with_range(0.0, 1000.0, 10.0);
    row.set_title(&tr("Output Delay"));
    row.set_subtitle(&tr("Milliseconds, for Bluetooth or slow speakers"));
    row.set_value(f64::from(live::output_delay_ms()));
    row.connect_value_notify(|r| {
        #[expect(clippy::cast_possible_truncation, reason = "the range is 0..=1000")]
        live::set_output_delay_ms(r.value().round() as i32);
    });
    row
}

fn boxed(rows: &[&gtk::Widget]) -> gtk::ListBox {
    let list = gtk::ListBox::new().class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for r in rows {
        list.append(*r);
    }
    list
}

fn join_entry() -> gtk::Box {
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("sdat://join/…"));
    entry.set_hexpand(true);
    let error = label("").class("error").class("caption").visible(false);
    error.set_wrap(true);
    let join = gtk::Button::with_label(&tr("Join")).class("suggested-action");
    let submit = {
        let (entry, error) = (entry.downgrade(), error.downgrade());
        move || {
            let (Some(entry), Some(error)) = (entry.upgrade(), error.upgrade()) else { return };
            match live::link::parse_invitation(&entry.text()) {
                Ok(url) => live::join(&url),
                Err(e) => {
                    log::info!("invalid invitation pasted error={e}");
                    error.set_text(&tr("This invitation link is not valid"));
                    error.set_visible(true);
                }
            }
        }
    };
    let s = submit.clone();
    join.connect_clicked(move |_| s());
    entry.connect_activate(move |_| submit());
    let row = spacing(hstack(&[entry.upcast_ref(), join.upcast_ref()]), 6);
    let caption = label(&tr("Join with Link")).class("heading").halign(gtk::Align::Start);
    spacing(vstack(&[caption.upcast_ref(), row.upcast_ref(), error.upcast_ref()]), 6)
}

fn qr_picture(url: &str) -> gtk::Widget {
    let img = gtk::Image::new();
    img.set_pixel_size(QR_SIZE);
    match render_qr(url) {
        Some(t) => img.set_paintable(Some(&t)),
        None => log::error!("could not render the Listen Together QR code"),
    }
    let frame = gtk::AspectFrame::new(0.5, 0.5, 1.0, true);
    frame.set_child(Some(&img));
    frame.background("alpha(var(--view-fg-color), 0.1)").halign(gtk::Align::Center).corner_radius(10).overflow_hidden().upcast()
}

fn hosting(url: &str, guests: &[String]) -> Vec<gtk::Widget> {
    let link = label(url).class("caption").class("dimmed");
    link.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    link.set_max_width_chars(28);
    link.set_selectable(true);
    let u = url.to_string();
    let copy = button(&tr("Copy Link"), "flat", move || {
        if let Some(d) = gdk::Display::default() {
            d.clipboard().set_text(&u);
            crate::player::toasts().notify(&tr("Invitation link copied"));
        }
    });
    let scan = wrapped(&tr("Scan this code with SDAT on your phone, or share the link")).class("dimmed");
    let rows: Vec<gtk::Widget> = if guests.is_empty() {
        vec![adw::ActionRow::builder().title(tr("No listeners yet")).build().upcast()]
    } else {
        guests
            .iter()
            .map(|g| {
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(g).as_str()).build();
                r.add_prefix(&gtk::Image::from_icon_name("person-symbolic"));
                r.upcast()
            })
            .collect()
    };
    let refs: Vec<&gtk::Widget> = rows.iter().collect();
    let end = button(&tr("End Session"), "destructive-action", live::end);
    vec![qr_picture(url), scan.upcast(), link.upcast(), copy.upcast(), boxed(&refs).upcast(), end.upcast()]
}

fn content(p: &Phase) -> gtk::Widget {
    let mut children: Vec<gtk::Widget> = vec![title(&tr("Listen Together")).upcast(), wrapped(&live::state::status_text(p)).class("dimmed").upcast()];
    match p {
        Phase::Idle => {
            children.push(button(&tr("Start Session"), "suggested-action", live::start_host).upcast());
            children.push(join_entry().upcast());
            children.push(boxed(&[output_delay_row().upcast_ref()]).upcast());
        }
        Phase::Starting | Phase::Joining => {
            children.push(adw::Spinner::new().size(32, 32).halign(gtk::Align::Center).upcast());
            children.push(button(&tr("Cancel"), "flat", live::end).upcast());
        }
        Phase::Hosting { url, guests } => children.extend(hosting(url, guests)),
        Phase::Listening { .. } | Phase::Reconnecting { .. } => {
            children.push(wrapped(&tr("The host controls playback. Music plays through your own TIDAL account.")).class("caption").upcast());
            children.push(boxed(&[output_delay_row().upcast_ref()]).upcast());
            children.push(button(&tr("Leave Session"), "destructive-action", live::end).upcast());
        }
        Phase::Ended(e) => {
            children.push(wrapped(&live::state::ended_message(e)).upcast());
            children.push(button(&tr("OK"), "suggested-action", live::dismiss).upcast());
        }
    }
    let refs: Vec<&gtk::Widget> = children.iter().collect();
    spacing(vstack(&refs), 12).margin(12).size(300, -1).upcast()
}

/// The queue view's Listen Together button, aligned to the end of the row
/// (the side opposite to shuffle in the player controls).
pub fn new_button() -> gtk::MenuButton {
    let bin = adw::Bin::new();
    on_state(live::phase(), &bin, |b, p| b.set_child(Some(&content(p))));
    let popover = gtk::Popover::new();
    popover.set_child(Some(&bin));
    let mb = gtk::MenuButton::new().class("flat").tooltip(&tr("Listen Together")).halign(gtk::Align::End);
    mb.set_icon_name("system-users-symbolic");
    mb.set_popover(Some(&popover));
    on_state(live::phase(), &mb, |m, p| {
        if p.is_active() {
            m.add_css_class("accent");
        } else {
            m.remove_css_class("accent");
        }
    });
    mb
}
