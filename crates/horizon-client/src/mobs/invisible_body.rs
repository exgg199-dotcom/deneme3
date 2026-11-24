//! Invisible bodies that remain visible to spectators or friendly teammates.
use super::{
    Atlas, Mob, MobData, Part, RenderedItem, Species, base_transform, body_color, render_visible,
    render_with_hidden,
};
use crate::mesher::Vertex;

#[allow(clippy::too_many_arguments)]
pub fn render_seen_invisible(
    m: &Mob,
    alpha: f32,
    camera: glam::DVec3,
    atlas: Atlas,
    light: u32,
    data: &MobData,
    extra: &[(&Part, [f32; 4], u32, u32, String, bool)],
    hidden: mod_api::HiddenParts,
    parts: Option<&mut Option<mod_api::PosedParts>>,
    out: &mut Vec<Vertex>,
) -> Vec<RenderedItem> {
    seen_invisible(
        m, alpha, camera, atlas, light, data, extra, hidden, parts, out, true,
    )
}
/// Game keeps sole ownership of the avatar cape layer when its body is translucent.
#[allow(clippy::too_many_arguments)]
pub fn render_avatar_seen_invisible(
    m: &Mob,
    alpha: f32,
    camera: glam::DVec3,
    atlas: Atlas,
    light: u32,
    data: &MobData,
    extra: &[(&Part, [f32; 4], u32, u32, String, bool)],
    out: &mut Vec<Vertex>,
) -> Vec<RenderedItem> {
    render_avatar_seen_invisible_with_hidden(
        m,
        alpha,
        camera,
        atlas,
        light,
        data,
        extra,
        Default::default(),
        None,
        out,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn render_avatar_seen_invisible_with_hidden(
    m: &Mob,
    alpha: f32,
    camera: glam::DVec3,
    atlas: Atlas,
    light: u32,
    data: &MobData,
    extra: &[(&Part, [f32; 4], u32, u32, String, bool)],
    hidden: mod_api::HiddenParts,
    parts: Option<&mut Option<mod_api::PosedParts>>,
    out: &mut Vec<Vertex>,
) -> Vec<RenderedItem> {
    seen_invisible(
        m, alpha, camera, atlas, light, data, extra, hidden, parts, out, false,
    )
}
#[allow(clippy::too_many_arguments)]
fn seen_invisible(
    m: &Mob,
    alpha: f32,
    camera: glam::DVec3,
    atlas: Atlas,
    light: u32,
    data: &MobData,
    extra: &[(&Part, [f32; 4], u32, u32, String, bool)],
    hidden: mod_api::HiddenParts,
    parts: Option<&mut Option<mod_api::PosedParts>>,
    out: &mut Vec<Vertex>,
    legacy_avatar_cape: bool,
) -> Vec<RenderedItem> {
    if m.species() == Species::Illusioner {
        return render_with_hidden(
            m, alpha, camera, atlas, light, data, extra, hidden, parts, out,
        );
    }
    let mut items = Vec::new();
    let tint = (body_color(m, alpha) & 0x00FF_FFFF) | 0x2600_0000;
    render_visible(
        m,
        alpha,
        base_transform(m, alpha, camera),
        atlas,
        light,
        data,
        extra,
        Some(tint),
        hidden,
        parts,
        out,
        &mut items,
        None,
        legacy_avatar_cape,
    );
    items
}
