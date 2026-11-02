//! Port of `components/sidebar/queue`.
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;

use crate::i18n::tr;
use crate::player::{self, QueueKind};
use crate::ui::components::{Col, TrackList};
use crate::ui::dsl::{W, vstack, on_state};

const SCROLL_MARGIN: f64 = 75.0;

fn empty_page() -> adw::StatusPage {
    adw::StatusPage::builder().title(tr("No Tracks in Queue")).icon_name("music-queue-empty-symbolic").vexpand(true).build()
}

fn entries(kind: QueueKind) -> &'static crate::signals::State<Vec<crate::model::Track>> {
    match kind {
        QueueKind::User => player::user_queue().entries(),
        QueueKind::Base => player::base_queue().entries(),
    }
}

fn queue_tracklist(kind: QueueKind) -> TrackList {
    let list = TrackList::new(vec![
        Col::cover(),
        Col::title_album(),
        Col::custom_widget_button(move |_, pos, _| {
            let b = gtk::Button::from_icon_name("user-trash-symbolic").tooltip(&tr("Remove Track from Queue")).class("flat").mend(10);
            b.connect_clicked(move |_| {
                let r = match kind {
                    QueueKind::User => player::user_queue().remove_at(pos),
                    QueueKind::Base => player::base_queue().remove_at(pos),
                };
                if let Err(e) = r {
                    log::warn!("failed to remove track from queue: {e}");
                }
            });
            b.upcast()
        }),
    ]);
    list.set_click_handler(move |_, pos| player::skip_through_queue(kind, pos));
    list.bind_tracks(entries(kind));
    list.set_reorder_callback(Some(Box::new(move |src, dst, track| {
        entries(kind).notify(|mut q| {
            if src < q.len() {
                q.remove(src);
            }
            let dst = dst.min(q.len());
            q.insert(dst, track.clone());
            q
        });
    })));
    list
}

fn queue_list() -> gtk::ScrolledWindow {
    let user = queue_tracklist(QueueKind::User);
    let base = queue_tracklist(QueueKind::Base);
    // The rest of the playing list that is not loaded yet shows as placeholder
    // rows; scrolling onto them loads the next page.
    base.on_placeholder_shown(|_| player::feed(true));
    let b = base.downgrade();
    let sub = player::list_unloaded().on(move |n| {
        let Some(b) = b.upgrade() else { return true };
        b.set_placeholders(*n);
        crate::signals::CONTINUE
    });
    base.widget().connect_destroy(move |_| player::list_unloaded().unsubscribe(sub));
    let user_w = user.widget().clone().background("alpha(var(--view-bg-color), 0.9)").corner_radius(10).mbottom(10);
    let content = vstack(&[&user_w, base.widget()]).valign(gtk::Align::Start);
    let sw = gtk::ScrolledWindow::new().hmargin(12).vexp(true);
    sw.set_child(Some(&content));
    // Auto-scroll while dragging near the edges.
    let action = Rc::new(Cell::new(0.0f64));
    let (a, w) = (Rc::clone(&action), sw.downgrade());
    glib::timeout_add_local(Duration::from_millis(10), move || {
        let Some(sw) = w.upgrade() else { return glib::ControlFlow::Break };
        if a.get() != 0.0 {
            let adj = sw.vadjustment();
            adj.set_value(adj.value() + a.get() * 10.0);
        }
        glib::ControlFlow::Continue
    });
    let motion = gtk::DropControllerMotion::new();
    let (a, w) = (Rc::clone(&action), sw.downgrade());
    motion.connect_motion(move |_, _, y| {
        let Some(sw) = w.upgrade() else { return };
        let h = f64::from(sw.height());
        if y < SCROLL_MARGIN {
            a.set((SCROLL_MARGIN - y) / SCROLL_MARGIN * -4.0);
        } else if h - y < SCROLL_MARGIN {
            a.set((SCROLL_MARGIN - (h - y)) / SCROLL_MARGIN * 4.0);
        } else {
            a.set(0.0);
        }
    });
    motion.connect_leave(move |_| action.set(0.0));
    sw.add_controller(motion);
    // Keep the tracklists alive with the scrolled window.
    sw.connect_destroy(move |_| {
        let _keep_alive = (&user, &base);
    });
    sw
}

pub fn new_queue() -> gtk::Box {
    let mini = super::mini_player();
    on_state(player::track_changed(), &mini, |m, t| m.set_visible(t.is_some()));
    let bin = adw::Bin::new();
    let list: Rc<std::cell::RefCell<Option<gtk::ScrolledWindow>>> = Rc::default();
    let update = move |bin: &adw::Bin| {
        let filled = !player::user_queue().is_empty() || !player::base_queue().is_empty();
        if filled {
            let sw = list.borrow_mut().get_or_insert_with(queue_list).clone();
            if bin.child().as_ref() != Some(sw.upcast_ref()) {
                bin.set_child(Some(&sw));
            }
        } else {
            bin.set_child(Some(&empty_page()));
        }
    };
    let update = Rc::new(update);
    let u = Rc::clone(&update);
    on_state(player::user_queue().entries(), &bin, move |b, _| u(b));
    on_state(player::base_queue().entries(), &bin, move |b, _| update(b));
    let listen = super::live::new_button().hmargin(12).mtop(6);
    vstack(&[listen.upcast_ref(), mini.upcast_ref(), bin.upcast_ref()])
}
