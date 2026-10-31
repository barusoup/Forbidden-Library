//! Google Photos-style square tiles: every cell is the same square size and
//! rows sit a couple of pixels apart, grouped under quiet date headers.

use archive_core::Item;
use dream::ui::Rect;

pub const HEADER_H: f32 = 38.0;

/// Preferred cell size. The columns are chosen from this and then share the
/// width, so there is one grid size and no way to make it denser.
pub const TILE_TARGET: f32 = 224.0;

#[derive(Clone, Debug)]
pub struct Tile {
    pub index: usize,
    pub rect: Rect,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub label: String,
    /// Content-space top of the header.
    pub top: f32,
    pub height: f32,
}

#[derive(Clone, Debug, Default)]
pub struct GridLayout {
    pub tiles: Vec<Tile>,
    pub groups: Vec<Group>,
    pub height: f32,
}

impl GridLayout {
    /// The date header pinned at the top of the viewport, and its y.
    pub fn sticky_header(&self, scroll: f32) -> Option<(&Group, f32)> {
        self.groups
            .iter()
            .find(|group| group.top <= scroll + 0.5 && scroll < group.top + group.height)
            .map(|group| (group, group.top))
            .or_else(|| self.groups.first().map(|group| (group, group.top)))
    }
}

/// The run of tiles whose content-space rectangles reach from `top` to
/// `bottom`. Tiles are laid out top to bottom, so both ends are found by binary
/// search instead of testing every tile in the grid on every frame.
pub fn tiles_between(tiles: &[Tile], top: f32, bottom: f32) -> std::ops::Range<usize> {
    let first = tiles.partition_point(|tile| tile.rect.y + tile.rect.h < top);
    let end = first + tiles[first..].partition_point(|tile| tile.rect.y <= bottom);
    first..end
}

/// Lay out `items` as squares under date headers. `target` is the preferred
/// cell size; the column count is the largest that keeps each cell at least
/// that big, and the cells then share the width so rows always end flush with
/// the edge.
pub fn build(items: &[Item], width: f32, target: f32, gap: f32) -> GridLayout {
    grouped(
        items,
        width,
        target,
        gap,
        // An unreadable timestamp still gets its own "Unknown date" header.
        &|item| Some(archive_core::local_day(item.captured_at).unwrap_or(i64::MIN)),
        &|item| archive_core::format_date(item.captured_at),
    )
}

/// Like [`build`], but with no date headers and no space reserved for them:
/// a page whose order is not the capture date, such as the trash.
pub fn build_flat(items: &[Item], width: f32, target: f32, gap: f32) -> GridLayout {
    grouped(items, width, target, gap, &|_| None, &|_| String::new())
}

/// Lays out `items` in rows, with a header for each run of items that `group`
/// gives the same key. An item that gives no group gets no header. The header
/// text comes from `label`, and is asked for once per run rather than per item.
fn grouped(
    items: &[Item],
    width: f32,
    target: f32,
    gap: f32,
    group: &dyn Fn(&Item) -> Option<i64>,
    label: &dyn Fn(&Item) -> String,
) -> GridLayout {
    let mut layout = GridLayout::default();
    let width = width.max(1.0);
    if items.is_empty() {
        return layout;
    }
    let target = target.max(40.0);
    let columns = (((width + gap) / (target + gap)).floor() as usize).max(1);
    let size = ((width - gap * columns.saturating_sub(1) as f32) / columns as f32).max(1.0);

    let mut y = 0.0;
    let mut index = 0;
    while index < items.len() {
        let header = group(&items[index]);
        let top = y;
        if header.is_some() {
            y += HEADER_H;
        }
        let mut cursor = index;
        while cursor < items.len() {
            // A run continues while items share its key. An unkeyed run takes
            // only unkeyed items, so a keyed item after it starts its own header.
            if group(&items[cursor]) != header {
                break;
            }
            let offset = cursor - index;
            let column = offset % columns;
            let row = offset / columns;
            layout.tiles.push(Tile {
                index: cursor,
                rect: Rect::new(
                    column as f32 * (size + gap),
                    y + row as f32 * (size + gap),
                    size,
                    size,
                ),
            });
            cursor += 1;
        }
        let rows = (cursor - index).div_ceil(columns);
        y += rows as f32 * (size + gap);
        if header.is_some() {
            layout.groups.push(Group {
                label: label(&items[index]),
                top,
                height: y - top,
            });
        }
        index = cursor;
    }
    layout.height = (y + 8.0).max(0.0);
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_core::{Item, Kind, OptState, ThumbState};

    fn item(width: u32, height: u32, day: i64) -> Item {
        Item {
            id: format!("{width}x{height}-{day}"),
            kind: Kind::Image,
            mime: "image/jpeg".into(),
            ext: "jpg".into(),
            original_ext: "jpg".into(),
            bytes: 1,
            original_bytes: 1,
            width: Some(width),
            height: Some(height),
            duration_ms: None,
            captured_at: day * 86_400_000,
            captured_source: "exif".into(),
            added_at: 0,
            modified_at: 0,
            original_name: "x.jpg".into(),
            rel_path: "store/x".into(),
            thumb_state: ThumbState::Ready,
            opt_state: OptState::None,
            opt_kind: None,
            deleted_at: None,
            camera_make: None,
            camera_model: None,
            lens: None,
            iso: None,
            f_number: None,
            exposure: None,
            focal_mm: None,
            orientation: 1,
            gps: None,
            avg_color: 0,
            checksum: "x".into(),
            content_hash: "x".into(),
            thumb_recipe: archive_core::THUMB_RECIPE,
            thumb_error: None,
            opt_error: None,
        }
    }

    #[test]
    fn tiles_between_finds_the_visible_run() {
        let tiles: Vec<Tile> = (0..10)
            .map(|row| Tile {
                index: row,
                rect: Rect::new(0.0, row as f32 * 100.0, 90.0, 90.0),
            })
            .collect();
        assert_eq!(tiles_between(&tiles, 250.0, 420.0), 2..5);
        assert_eq!(tiles_between(&tiles, -500.0, -100.0), 0..0);
        assert_eq!(tiles_between(&tiles, 0.0, 5000.0), 0..10);
    }

    #[test]
    fn tiles_are_square_and_rows_fill_the_width() {
        let items: Vec<Item> = (0..12)
            .map(|index| item(1200 + index * 40, 800, 20_000))
            .collect();
        let layout = build(&items, 1000.0, 220.0, 2.0);
        assert_eq!(layout.tiles.len(), 12);
        let columns = (1002.0f32 / 222.0).floor() as usize;
        assert_eq!(columns, 4);
        for tile in &layout.tiles {
            assert!((tile.rect.w - tile.rect.h).abs() < 0.01);
        }
        for row in layout.tiles.chunks(columns) {
            let last = row.last().unwrap();
            assert!((last.rect.x + last.rect.w - 1000.0).abs() < 0.5);
            for pair in row.windows(2) {
                assert!((pair[1].rect.x - pair[0].rect.max().0 - 2.0).abs() < 0.01);
            }
        }
    }

    #[test]
    fn groups_follow_dates() {
        let items = vec![
            item(800, 600, 19_000),
            item(800, 600, 19_000),
            item(800, 600, 20_000),
        ];
        let layout = build(&items, 900.0, 200.0, 2.0);
        assert_eq!(layout.groups.len(), 2);
        assert!(layout.groups[1].top > layout.groups[0].top);
        let second = &layout.tiles[2];
        assert!(second.rect.y >= layout.groups[1].top + HEADER_H - 0.01);
    }

    #[test]
    fn a_flat_grid_has_no_headers_or_header_space() {
        let items = vec![
            item(800, 600, 19_000),
            item(800, 600, 19_000),
            item(800, 600, 20_000),
        ];
        let layout = build_flat(&items, 900.0, 200.0, 2.0);
        assert!(layout.groups.is_empty());
        assert_eq!(layout.tiles.len(), 3);
        assert_eq!(layout.tiles[0].rect.y, 0.0);
        assert!(layout.sticky_header(0.0).is_none());
    }

    #[test]
    fn a_panorama_keeps_the_square_cell() {
        let items = vec![item(6000, 1000, 20_000)];
        let layout = build(&items, 1000.0, 220.0, 2.0);
        assert_eq!(layout.tiles.len(), 1);
        let tile = &layout.tiles[0];
        assert!((tile.rect.w - tile.rect.h).abs() < 0.01);
        assert!(tile.rect.w <= 1000.1);
    }

    #[test]
    fn a_narrow_window_falls_back_to_one_column() {
        let items: Vec<Item> = (0..3).map(|_| item(800, 600, 20_000)).collect();
        let layout = build(&items, 120.0, 220.0, 2.0);
        let xs: Vec<f32> = layout.tiles.iter().map(|tile| tile.rect.x).collect();
        assert!(xs.iter().all(|x| *x == 0.0));
        assert!((layout.tiles[0].rect.w - 120.0).abs() < 0.01);
    }
}
