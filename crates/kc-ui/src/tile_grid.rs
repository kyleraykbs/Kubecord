//! The grid a call's tiles sit in: every tile the same 16:9 size, as large
//! as fits the room there is, with a short last row centred. The number of
//! columns follows the shape of the room, so the grid rearranges as the
//! window (or the call's share of it) is resized.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Room between tiles.
pub(crate) const SPACING: i32 = 8;
/// The smallest a tile gets before the grid scrolls instead of shrinking.
pub(crate) const MIN_TILE: (i32, i32) = (160, 90);

/// How `count` tiles are laid out in a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Arrangement {
    pub columns: usize,
    pub rows: usize,
    pub tile: (i32, i32),
}

/// The most columns of minimum-size tiles that fit `width`.
fn columns_at_minimum(count: usize, width: i32) -> usize {
    (((width + SPACING) / (MIN_TILE.0 + SPACING)).max(1) as usize).min(count.max(1))
}

/// The arrangement with the largest tiles that fits `width` by `height`.
/// When even minimum-size tiles do not fit, they stay that size in as many
/// columns as the width takes and the grid is taller than the room.
pub(crate) fn arrange(count: usize, width: i32, height: i32) -> Arrangement {
    let count = count.max(1);
    let mut best: Option<Arrangement> = None;
    for columns in 1..=count {
        let rows = count.div_ceil(columns);
        let across = SPACING * (columns as i32 - 1);
        let down = SPACING * (rows as i32 - 1);
        let mut tile_width = (width - across) / columns as i32;
        let mut tile_height = tile_width * 9 / 16;
        if tile_height * rows as i32 + down > height {
            tile_height = (height - down) / rows as i32;
            tile_width = tile_height * 16 / 9;
        }
        if best.is_none_or(|best| tile_width > best.tile.0) {
            best = Some(Arrangement {
                columns,
                rows,
                tile: (tile_width, tile_height),
            });
        }
    }
    match best {
        Some(best) if best.tile.0 >= MIN_TILE.0 && best.tile.1 >= MIN_TILE.1 => best,
        _ => {
            let columns = columns_at_minimum(count, width);
            Arrangement {
                columns,
                rows: count.div_ceil(columns),
                tile: MIN_TILE,
            }
        }
    }
}

/// Where tile `index` of `count` goes: `(x, y)` of its top-left corner.
pub(crate) fn place(
    arrangement: Arrangement,
    count: usize,
    index: usize,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let (tile_width, tile_height) = arrangement.tile;
    let row = index / arrangement.columns;
    let column = index % arrangement.columns;
    // The last row may be short; it is centred like the rest.
    let in_row = if row + 1 == arrangement.rows {
        count - row * arrangement.columns
    } else {
        arrangement.columns
    } as i32;
    let row_width = in_row * tile_width + SPACING * (in_row - 1);
    let rows = arrangement.rows as i32;
    let grid_height = rows * tile_height + SPACING * (rows - 1);
    let x = (width - row_width) / 2 + column as i32 * (tile_width + SPACING);
    let y = ((height - grid_height) / 2).max(0) + row as i32 * (tile_height + SPACING);
    (x.max(0), y)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TileGrid;

    #[glib::object_subclass]
    impl ObjectSubclass for TileGrid {
        const NAME: &'static str = "KcTileGrid";
        type Type = super::TileGrid;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TileGrid {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for TileGrid {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let count = self.obj().observe_children().n_items() as usize;
            match orientation {
                gtk::Orientation::Horizontal => (MIN_TILE.0, MIN_TILE.0, -1, -1),
                _ => {
                    // The least height is all the tiles at their minimum
                    // size; past that the grid's scroller takes over.
                    let rows = if for_size < 0 {
                        1
                    } else {
                        count.max(1).div_ceil(columns_at_minimum(count, for_size))
                    } as i32;
                    let least = rows * MIN_TILE.1 + SPACING * (rows - 1);
                    (least, least, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let widget = self.obj();
            let mut children = Vec::new();
            let mut next = widget.first_child();
            while let Some(child) = next {
                next = child.next_sibling();
                if child.should_layout() {
                    children.push(child);
                }
            }
            let arrangement = arrange(children.len(), width, height);
            for (index, child) in children.iter().enumerate() {
                let (x, y) = place(arrangement, children.len(), index, width, height);
                // A widget must be measured before it is given a size.
                let _ = child.measure(gtk::Orientation::Horizontal, -1);
                child.size_allocate(
                    &gtk::Allocation::new(x, y, arrangement.tile.0, arrangement.tile.1),
                    -1,
                );
            }
        }
    }
}

glib::wrapper! {
    pub struct TileGrid(ObjectSubclass<imp::TileGrid>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TileGrid {
    pub(crate) fn new() -> Self {
        let grid: Self = glib::Object::new();
        grid.set_overflow(gtk::Overflow::Hidden);
        grid
    }

    pub(crate) fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
    }
}

impl Default for TileGrid {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_grow_to_fill_the_room_in_whatever_shape_it_has() {
        // One tile takes the whole width of a wide room, at 16:9.
        let one = arrange(1, 1600, 1000);
        assert_eq!((one.columns, one.tile), (1, (1600, 900)));
        // Six in a wide room: three across, two down, bounded by height.
        let wide = arrange(6, 1600, 600);
        assert_eq!((wide.columns, wide.rows), (3, 2));
        assert_eq!(wide.tile.1, (600 - SPACING) / 2);
        // The same six in a tall room: two across, three down.
        let tall = arrange(6, 600, 800);
        assert_eq!((tall.columns, tall.rows), (2, 3));
        assert_eq!(tall.tile.0, (600 - SPACING) / 2);
        // In a strip, all six side by side.
        assert_eq!(arrange(6, 2000, 150).columns, 6);
    }

    #[test]
    fn tiles_stop_shrinking_at_the_minimum() {
        // Twelve tiles cannot fit 400x200 at any useful size: two columns
        // of minimum tiles, taller than the room.
        let cramped = arrange(12, 400, 200);
        assert_eq!(cramped.tile, MIN_TILE);
        assert_eq!((cramped.columns, cramped.rows), (2, 6));
    }

    #[test]
    fn a_short_last_row_is_centred() {
        // Five tiles, three across: the last two sit in the middle.
        let arrangement = arrange(5, 1000, 400);
        assert_eq!((arrangement.columns, arrangement.rows), (3, 2));
        let (first_x, first_y) = place(arrangement, 5, 0, 1000, 400);
        let (fourth_x, fourth_y) = place(arrangement, 5, 3, 1000, 400);
        let (fifth_x, _) = place(arrangement, 5, 4, 1000, 400);
        let tile = arrangement.tile.0;
        assert!(fourth_y > first_y);
        assert!(fourth_x > first_x);
        // Centred: as much room left of the fourth as right of the fifth.
        assert!((fourth_x - (1000 - (fifth_x + tile))).abs() <= 1);
    }
}
