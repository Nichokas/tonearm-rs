//! Port of `components/sidebar`.
pub mod live;
pub mod lyrics;
pub mod player_view;
pub mod queue;

use adw::prelude::*;
use gtk::pango;

use crate::i18n::tr;
use crate::player::{self, PlaybackStatus};
use crate::ui::dsl::{W, label, on_state, spacing, vstack, hstack, spacer};
use crate::imgutil;

/// Port of `MiniPlayer`.
pub fn mini_player() -> adw::Bin {
    let cover = gtk::Image::from_paintable(Some(&imgutil::missing_album()));
    cover.set_pixel_size(54);
    let frame = gtk::AspectFrame::new(0.5, 0.5, 1.0, true);
    frame.set_child(Some(&cover));
    let frame = frame.overflow_hidden().background("alpha(var(--view-fg-color), 0.1)").corner_radius(6);
    let title = label("").font_weight(600).halign(gtk::Align::Start);
    title.set_ellipsize(pango::EllipsizeMode::End);
    let album = label("").class("dimmed").halign(gtk::Align::Start);
    album.set_ellipsize(pango::EllipsizeMode::End);
    on_state(player::track_changed(), &cover, {
        let (title, album) = (title.downgrade(), album.downgrade());
        move |img, ev| {
            let Some(ev) = ev else { return };
            let c = ev.track.cover(80);
            if !c.has_static() {
                log::error!("Failed to load cover URL");
                return;
            }
            let w = img.downgrade();
            glib::spawn_future_local(async move {
                if let (Some(t), Some(i)) = (imgutil::fetch(&c.static_).await, w.upgrade()) {
                    i.set_paintable(Some(&t));
                }
            });
            if let Some(t) = title.upgrade() {
                t.set_text(&ev.track.formatted_title());
            }
            if let Some(a) = album.upgrade() {
                a.set_text(&ev.track.album.as_ref().map(|a| a.title.clone()).unwrap_or_default());
            }
        }
    });
    let texts = spacing(vstack(&[title.upcast_ref(), album.upcast_ref()]), 3).valign(gtk::Align::Center).halign(gtk::Align::Start).hexp(true);
    let pp = gtk::Button::from_icon_name("play-symbolic").tooltip(&tr("Play / Pause")).class("flat");
    pp.connect_clicked(|_| player::play_pause());
    on_state(player::playback_state_changed(), &pp, |b, ps| {
        b.set_sensitive(!ps.loading);
        if ps.loading {
            b.set_child(Some(&adw::Spinner::new()));
        } else {
            b.set_icon_name(if ps.status == PlaybackStatus::Playing { "pause-symbolic" } else { "play-symbolic" });
        }
    });
    let next = gtk::Button::from_icon_name("skip-forward-large-symbolic").tooltip(&tr("Next")).class("flat");
    next.set_action_name(Some("win.player.next"));
    on_state(player::playback_state_changed(), &next, |b, ps| b.set_sensitive(!ps.loading));
    let buttons = spacing(hstack(&[pp.upcast_ref(), next.upcast_ref()]), 7).halign(gtk::Align::End).valign(gtk::Align::Center);
    let row = spacing(hstack(&[frame.upcast_ref(), texts.upcast_ref(), spacer().vexp(false).upcast_ref(), buttons.upcast_ref()]), 16)
        .padding(10)
        .margin(2)
        .background("alpha(var(--view-fg-color), 0.1)")
        .corner_radius(12);
    let bin = adw::Bin::new().margin(12);
    bin.set_child(Some(&row));
    bin
}

