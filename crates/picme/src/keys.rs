//! Raccourcis clavier : la table touche -> action, sans effet de bord (l'application des actions est dans `main`).
//! Les touches du bloc 6 (C, Entree du crop, Espace de la video) n'y sont pas encore.

use gtk4::gdk::{self, Key, ModifierType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Image suivante ou precedente (une touche maintenue repete l'action).
    Next,
    Prev,
    First,
    Last,
    /// Fit <-> 100 %.
    ToggleZoom,
    ZoomIn,
    ZoomOut,
    Fit,
    /// Quart de tour visuel de l'affichage (le fichier n'est pas touche).
    RotateRight,
    RotateLeft,
    /// R et Maj+R : quart de tour du fichier, horaire ou anti-horaire (vue seule si le format ne s'ecrit pas).
    TurnRight,
    TurnLeft,
    /// Suppr : le courant (et le `.mov` d'une Live Photo) a la corbeille.
    Trash,
    /// Maj+Suppr : suppression definitive, sans confirmation.
    Delete,
    Fullscreen,
    Quit,
}

/// L'action d'une touche pressee avec les modificateurs `state`. Ctrl+gauche et Ctrl+droite tournent la vue ; tout
/// autre Ctrl ou Alt est laisse a GTK ; Maj ne change rien, sauf pour R et Suppr.
pub fn action(key: Key, state: ModifierType) -> Option<Action> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
    if state.contains(gdk::ModifierType::ALT_MASK) || state.contains(gdk::ModifierType::SUPER_MASK) {
        return None;
    }
    if ctrl {
        return match key {
            Key::Right => Some(Action::RotateRight),
            Key::Left => Some(Action::RotateLeft),
            _ => None,
        };
    }
    Some(match key.to_lower() {
        Key::Right | Key::space | Key::Page_Down => Action::Next,
        Key::Left | Key::BackSpace | Key::Page_Up => Action::Prev,
        Key::Home => Action::First,
        Key::End => Action::Last,
        Key::f => Action::ToggleZoom,
        Key::plus | Key::equal | Key::KP_Add => Action::ZoomIn,
        Key::minus | Key::KP_Subtract => Action::ZoomOut,
        Key::_0 | Key::KP_0 => Action::Fit,
        // Maj et pas la casse : le verrouillage majuscule ne change pas le sens
        Key::r if shift => Action::TurnLeft,
        Key::r => Action::TurnRight,
        Key::Delete | Key::KP_Delete if shift => Action::Delete,
        Key::Delete | Key::KP_Delete => Action::Trash,
        Key::F11 => Action::Fullscreen,
        Key::q | Key::Escape => Action::Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifierType = ModifierType::empty();

    #[test]
    fn navigation_keys() {
        assert_eq!(action(Key::Right, NONE), Some(Action::Next));
        assert_eq!(action(Key::space, NONE), Some(Action::Next));
        assert_eq!(action(Key::Left, NONE), Some(Action::Prev));
        assert_eq!(action(Key::BackSpace, NONE), Some(Action::Prev));
        assert_eq!(action(Key::Home, NONE), Some(Action::First));
        assert_eq!(action(Key::End, NONE), Some(Action::Last));
        // Maj ne change pas la navigation
        assert_eq!(action(Key::Right, ModifierType::SHIFT_MASK), Some(Action::Next));
    }

    #[test]
    fn f_toggles_the_zoom_and_f11_is_the_fullscreen() {
        assert_eq!(action(Key::f, NONE), Some(Action::ToggleZoom));
        assert_eq!(action(Key::F, ModifierType::SHIFT_MASK), Some(Action::ToggleZoom), "verr. maj");
        assert_eq!(action(Key::F11, NONE), Some(Action::Fullscreen));
    }

    #[test]
    fn quit_keys_and_zoom_keys() {
        assert_eq!(action(Key::q, NONE), Some(Action::Quit));
        assert_eq!(action(Key::Q, ModifierType::SHIFT_MASK), Some(Action::Quit));
        assert_eq!(action(Key::Escape, NONE), Some(Action::Quit));
        assert_eq!(action(Key::plus, NONE), Some(Action::ZoomIn));
        assert_eq!(action(Key::minus, NONE), Some(Action::ZoomOut));
        assert_eq!(action(Key::_0, NONE), Some(Action::Fit));
    }

    #[test]
    fn ctrl_arrows_rotate_the_view_and_other_ctrl_alt_combinations_are_left_alone() {
        let ctrl = ModifierType::CONTROL_MASK;
        assert_eq!(action(Key::Right, ctrl), Some(Action::RotateRight));
        assert_eq!(action(Key::Left, ctrl), Some(Action::RotateLeft));
        assert_eq!(action(Key::q, ctrl), None, "Ctrl+Q n'est pas Q");
        assert_eq!(action(Key::f, ctrl), None);
        assert_eq!(action(Key::Right, ModifierType::ALT_MASK), None);
    }

    #[test]
    fn r_turns_the_file_and_delete_trashes_or_deletes_with_shift() {
        let shift = ModifierType::SHIFT_MASK;
        assert_eq!(action(Key::r, NONE), Some(Action::TurnRight));
        assert_eq!(action(Key::R, shift), Some(Action::TurnLeft));
        assert_eq!(action(Key::R, ModifierType::LOCK_MASK), Some(Action::TurnRight), "verr. maj");
        assert_eq!(action(Key::Delete, NONE), Some(Action::Trash));
        assert_eq!(action(Key::KP_Delete, NONE), Some(Action::Trash));
        assert_eq!(action(Key::Delete, shift), Some(Action::Delete));
        assert_eq!(action(Key::Delete, ModifierType::CONTROL_MASK), None);
        assert_eq!(action(Key::r, ModifierType::CONTROL_MASK), None);
    }

    #[test]
    fn keys_of_block_six_are_not_bound_yet() {
        for k in [Key::c, Key::Return] {
            assert_eq!(action(k, NONE), None, "{k:?}");
            assert_eq!(action(k, ModifierType::SHIFT_MASK), None, "{k:?}");
        }
    }
}
