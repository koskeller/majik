//! Album picker sheet: toggle membership of the selected items in each
//! album, or create a new album inline.

use gpui::{prelude::*, px, App, Entity, SharedString, Window};
use gpui_component::button::{ButtonVariants as _};
use crate::ui::button;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Sizable as _, WindowExt as _};
use majik_core::model::{AlbumId, GenerationId};

use crate::state::{self, LibraryModel};

pub fn open_album_picker(ids: Vec<GenerationId>, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("New album name"));
    let library = state::library(cx);
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let ids = ids.clone();
        let library = library.clone();
        let input = input.clone();
        let title = if ids.len() == 1 { "Add to Album".to_string() } else { format!("Add {} items to Album", ids.len()) };
        dialog.title(title).w(px(420.)).content(move |content, _, cx| content.py_1().child(render(&library, &input, ids.clone(), cx)))
    });
}

fn render(library: &Entity<LibraryModel>, input: &Entity<InputState>, ids: Vec<GenerationId>, cx: &mut App) -> impl IntoElement {
    let theme = cx.theme();
    let muted_fg = theme.muted_foreground;
    let albums: Vec<(AlbumId, String, bool)> = library
        .read(cx)
        .lib
        .albums()
        .iter()
        .map(|a| (a.id.clone(), a.name.clone(), ids.iter().all(|id| a.items.contains(id))))
        .collect();

    let mut list = v_flex().gap_1();
    if albums.is_empty() {
        list = list.child(gpui::div().text_sm().text_color(muted_fg).child("No albums yet — create one below."));
    }
    for (album_id, name, checked) in albums {
        let lib = library.clone();
        let ids = ids.clone();
        list = list.child(
            Checkbox::new(SharedString::from(format!("album-{}", album_id.0))).cursor_pointer().label(name).checked(checked).on_click(move |checked: &bool, _, cx| {
                let on = *checked;
                lib.update(cx, |m, cx| {
                    if on {
                        m.add_to_album(&album_id, &ids, cx);
                    } else {
                        m.remove_from_album(&album_id, &ids, cx);
                    }
                });
            }),
        );
    }

    let lib = library.clone();
    let input_create = input.clone();
    let ids_create = ids.clone();
    let create_row = h_flex().gap_2().items_center().child(gpui::div().flex_1().child(Input::new(input))).child(
        button("create-album").label("Create").primary().small().on_click(move |_, window, cx| {
            let name = input_create.read(cx).value().trim().to_string();
            if name.is_empty() {
                return;
            }
            let ids = ids_create.clone();
            lib.update(cx, |m, cx| {
                let id = m.create_album(name, cx);
                m.add_to_album(&id, &ids, cx);
            });
            input_create.update(cx, |s, cx| s.set_value("", window, cx));
        }),
    );

    v_flex()
        .gap_3()
        .child(gpui::div().id("album-list").max_h(px(320.)).overflow_y_scrollbar().child(list))
        .child(create_row)
        .child(h_flex().justify_end().child(button("done").label("Done").small().on_click(|_, window, cx| window.close_dialog(cx))))
}
