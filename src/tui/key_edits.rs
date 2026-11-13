//! What the key edits ask of `App` (spec "Edits on the Key detail (P3)", `N` on the key
//! list). The views can't move between screens or reach each other, so after every
//! call into the views `App` takes what they left here ([`App::after_key_edits`]):
//!
//! - `$EDITOR` to run for a value too big for the inline prompt ([`Effect::Editor`]);
//! - after `D`, back to the key list, without the deleted key;
//! - after `R`, the key list's row renamed; after any write, its TTL and length;
//! - after `N`, the new key in the key list and its detail open.

use super::views::value::Nav;
use super::{App, Effect, Owner, Screen};
use crate::live::keys::KeyRow;
use crate::value::Raw;

impl App {
    /// Takes what the key detail and key list asked for, and does it.
    pub(super) fn after_key_edits(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(job) = self.views.value.editor.take() {
            out.push(Effect::Editor(job));
        }
        for nav in std::mem::take(&mut self.views.value.nav) {
            out.extend(self.value_nav(nav));
        }
        if let Some(row) = self.views.keys.opened.take() {
            out.extend(self.open_new_key(row));
        }
        out
    }

    fn value_nav(&mut self, nav: Nav) -> Vec<Effect> {
        match nav {
            Nav::Deleted(key) => {
                self.views.keys.drop_key(&key);
                if self.screen() == Screen::KeyDetail {
                    return self.switch(Screen::KeyList);
                }
                // Coming back to Patterns shows the key list, not the deleted key.
                self.patterns.detail_open = false;
                Vec::new()
            }
            Nav::Renamed { old, new } => {
                self.views.keys.rename_key(&old, &new);
                Vec::new()
            }
            Nav::Row(row) => {
                self.views.keys.update_row(&row);
                Vec::new()
            }
        }
    }

    /// `N` added `row`: list it, and show it when the key list is still on screen.
    fn open_new_key(&mut self, row: KeyRow) -> Vec<Effect> {
        self.views.keys.add_row(row.clone());
        if self.screen() != Screen::KeyList {
            return Vec::new();
        }
        let pattern = self.views.keys.pattern.clone().unwrap_or_default();
        let mut out = self
            .with_views(Owner::Value, "open", |v, act| {
                v.value.open(row, pattern, act)
            })
            .map_or_else(Vec::new, |(_, e)| e);
        out.extend(self.switch(Screen::KeyDetail));
        out
    }

    /// [`super::Msg::Edited`]: `$EDITOR` ended.
    pub(super) fn edited(&mut self, job: u64, result: Result<Raw, String>) -> Vec<Effect> {
        self.with_views(Owner::Value, "edited", |v, act| {
            v.value.edited(job, result, act)
        })
        .map_or_else(Vec::new, |(_, e)| e)
    }
}

#[cfg(test)]
mod tests {
    use super::super::views::testing;
    use super::super::views::value::ValueState;
    use super::*;
    use crate::live::LiveRequest;
    use crate::model::Env;

    /// 4 Streams and 6 Search show a value with the key detail's state, but its edit
    /// keys are the key detail's only.
    #[test]
    fn edit_keys_do_nothing_where_the_value_is_embedded() {
        let (mut app, _) = testing::app_on(Screen::Streams);
        let s = app.session.as_mut().unwrap();
        s.profile.env = Env::Local;
        s.profile.read_only = None;
        let mut state = ValueState::default();
        state.row = Some(testing::key_row());
        for k in ['e', '+', 't', 'R', 'd', 'D'] {
            let key = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(k),
                crossterm::event::KeyModifiers::NONE,
            );
            let (done, effects) = app
                .with_views(Owner::Streams, "key", |_, act| {
                    super::super::views::ViewState::on_key(&mut state, key, act)
                })
                .unwrap();
            assert!(!done, "{k}");
            assert!(state.form.is_none(), "{k}");
            assert!(
                !effects.iter().any(|e| matches!(
                    e,
                    Effect::Live {
                        req: LiveRequest::Write(_),
                        ..
                    }
                )),
                "{k}"
            );
            assert!(app.confirm.is_none(), "{k}");
        }
    }
}
