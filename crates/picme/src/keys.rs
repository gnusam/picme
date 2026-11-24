//! Raccourcis clavier : la table touche -> action, sans effet de bord (l'application des actions est dans `main`).
//! Le contexte (`Ctx`) change le sens de quelques touches : Espace et Maj+fleches sur une video, tout pendant un crop.

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
    /// Maj+Suppr : suppression, definitive au bout de 30 s sans Ctrl+Z.
    Delete,
    /// Ctrl+Z : remet le dernier fichier retire, tant que les 30 s ne sont pas passees.
    Undo,
    Fullscreen,
    /// O : ouvre le dossier courant dans le gestionnaire de fichiers du systeme.
    OpenFolder,
    /// ? ou F1 : panneau d'aide des raccourcis.
    Help,
    Quit,
    /// C : cadre de recadrage sur l'image.
    Crop,
    /// Echap pendant le crop : abandon.
    CropCancel,
    /// Entree pendant le crop : copie `<nom>-crop.<ext>` ; Maj+Entree (`replace`) : remplace l'original, qui part a
    /// la corbeille.
    CropApply { replace: bool },
    /// Espace sur une video : pause et reprise.
    Pause,
    /// Maj+droite et Maj+gauche sur une video : saut de `video::SEEK_STEP` secondes.
    SeekForward,
    SeekBack,
    /// M : son de la video, coupe par defaut.
    Mute,
    /// L : tant qu'elle est maintenue, la Live Photo joue son `.mov`.
    Live,
}

/// Ce que la vue montre, pour les touches dont le sens en depend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ctx {
    /// Le courant est une video.
    pub video: bool,
    /// Le cadre de crop est ouvert : seules Echap, Entree et deux touches de fenetre agissent.
    pub crop: bool,
}

/// L'action d'une touche pressee avec les modificateurs `state`. Ctrl+gauche et Ctrl+droite tournent la vue ; tout
/// autre Ctrl ou Alt est laisse a GTK ; Maj ne change rien, sauf pour R, Suppr et, sur une video, les fleches.
pub fn action(key: Key, state: ModifierType, ctx: Ctx) -> Option<Action> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
    if state.contains(gdk::ModifierType::ALT_MASK) || state.contains(gdk::ModifierType::SUPER_MASK) {
        return None;
    }
    if ctx.crop {
        return match key {
            Key::Escape => Some(Action::CropCancel),
            Key::Return | Key::KP_Enter | Key::ISO_Enter => Some(Action::CropApply { replace: shift }),
            Key::F11 => Some(Action::Fullscreen),
            _ if ctrl => None,
            _ if key.to_lower() == Key::q => Some(Action::Quit),
            _ => None,
        };
    }
    if ctrl {
        return match key {
            Key::Right => Some(Action::RotateRight),
            Key::Left => Some(Action::RotateLeft),
            _ if key.to_lower() == Key::z && !shift => Some(Action::Undo),
            _ => None,
        };
    }
    if ctx.video {
        match key {
            Key::space => return Some(Action::Pause),
            Key::Right if shift => return Some(Action::SeekForward),
            Key::Left if shift => return Some(Action::SeekBack),
            _ => {}
        }
    }
    Some(match key.to_lower() {
        Key::Right | Key::space | Key::Page_Down => Action::Next,
        Key::Left | Key::BackSpace | Key::Page_Up => Action::Prev,
        Key::c => Action::Crop,
        Key::m => Action::Mute,
        Key::l => Action::Live,
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
        Key::o => Action::OpenFolder,
        Key::question | Key::F1 => Action::Help,
        Key::q | Key::Escape => Action::Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifierType = ModifierType::empty();
    const SHIFT: ModifierType = ModifierType::SHIFT_MASK;

    /// Le cas courant : une image, pas de crop.
    fn action(key: Key, state: ModifierType) -> Option<Action> {
        super::action(key, state, Ctx::default())
    }

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
        assert_eq!(action(Key::o, NONE), Some(Action::OpenFolder));
        assert_eq!(action(Key::question, SHIFT), Some(Action::Help));
        assert_eq!(action(Key::F1, NONE), Some(Action::Help));
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
        assert_eq!(action(Key::z, ModifierType::CONTROL_MASK), Some(Action::Undo));
        assert_eq!(action(Key::Z, ModifierType::CONTROL_MASK | shift), None, "Ctrl+Maj+Z : pas de retablir");
        assert_eq!(action(Key::z, NONE), None);
        assert_eq!(action(Key::r, ModifierType::CONTROL_MASK), None);
    }

    #[test]
    fn c_opens_the_crop_and_m_and_l_are_bound_on_every_view() {
        assert_eq!(action(Key::c, NONE), Some(Action::Crop));
        assert_eq!(action(Key::C, ModifierType::LOCK_MASK), Some(Action::Crop), "verr. maj");
        assert_eq!(action(Key::c, ModifierType::CONTROL_MASK), None, "Ctrl+C reste a GTK");
        assert_eq!(action(Key::m, NONE), Some(Action::Mute));
        assert_eq!(action(Key::l, NONE), Some(Action::Live));
        // sans crop, Entree ne fait rien
        assert_eq!(action(Key::Return, NONE), None);
    }

    #[test]
    fn space_and_shift_arrows_drive_a_video_and_plain_arrows_keep_navigating() {
        let video = Ctx { video: true, crop: false };
        assert_eq!(super::action(Key::space, NONE, video), Some(Action::Pause));
        assert_eq!(super::action(Key::Right, SHIFT, video), Some(Action::SeekForward));
        assert_eq!(super::action(Key::Left, SHIFT, video), Some(Action::SeekBack));
        assert_eq!(super::action(Key::Right, NONE, video), Some(Action::Next));
        assert_eq!(super::action(Key::Left, NONE, video), Some(Action::Prev));
        assert_eq!(super::action(Key::Page_Down, NONE, video), Some(Action::Next));
        // Ctrl+fleches tournent la vue, video ou pas
        assert_eq!(super::action(Key::Right, ModifierType::CONTROL_MASK, video), Some(Action::RotateRight));
        // sur une image, Espace avance et Maj+fleche navigue
        assert_eq!(action(Key::space, NONE), Some(Action::Next));
        assert_eq!(action(Key::Right, SHIFT), Some(Action::Next));
    }

    #[test]
    fn during_a_crop_only_escape_enter_and_the_window_keys_act() {
        let crop = Ctx { video: false, crop: true };
        assert_eq!(super::action(Key::Escape, NONE, crop), Some(Action::CropCancel));
        assert_eq!(super::action(Key::Return, NONE, crop), Some(Action::CropApply { replace: false }));
        assert_eq!(super::action(Key::KP_Enter, NONE, crop), Some(Action::CropApply { replace: false }));
        assert_eq!(super::action(Key::Return, SHIFT, crop), Some(Action::CropApply { replace: true }));
        assert_eq!(super::action(Key::F11, NONE, crop), Some(Action::Fullscreen));
        assert_eq!(super::action(Key::q, NONE, crop), Some(Action::Quit));
        // rien de ce qui change l'image sous le cadre
        for k in [Key::Right, Key::Left, Key::space, Key::r, Key::Delete, Key::c, Key::plus, Key::f, Key::Home] {
            assert_eq!(super::action(k, NONE, crop), None, "{k:?}");
        }
        assert_eq!(super::action(Key::Delete, SHIFT, crop), None);
        assert_eq!(super::action(Key::Right, ModifierType::CONTROL_MASK, crop), None);
    }
}
