"""Exercise production menu ownership across all guest menu storage paths."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_world_input_capture import method

ROOT = Path(__file__).resolve().parents[1]


class GuestMenuWindowTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('rustc'), 'requires Rust')
    def test_only_the_open_menu_accepts_addressed_actions(self):
        source = (ROOT / 'crates/horizon-client/src/game/network/lan_actions/packets.rs').read_text()
        window = method((ROOT / 'crates/horizon-client/src/game/network/lan_actions/menus.rs').read_text(), 'guest_menu_window')
        start = source.index('        let addressed_window = match &packet')
        end = source.index('        if matches!(', start)
        gate = source[start:end].replace('return;', 'return false;')
        fixture = r'''
use std::collections::HashMap;
struct Menu { window: i32 }
struct Guest { menu: Option<Menu> }
struct Hub { g: Guest }
impl Hub { fn guest(&self, _: u32) -> Option<&Guest> { Some(&self.g) } }
#[derive(Default)] struct Crafting { table: Option<(i32,)> }
#[derive(Default)] struct Play { crafting: Crafting, station: Option<Menu>, merchant: Option<Menu>, lectern: Option<Menu>, mount_window: Option<Menu> }
struct LanHost { hub: Hub, play: HashMap<u32, Play>, menus:HashMap<u32, Play> }
struct Packet { container: i32 }
enum S { ContainerClick(Packet), ContainerButtonClick(Packet), ContainerSlotStateChanged(Packet), PlaceRecipe(Packet), Other }
struct Game{lan:Option<LanHost>}
impl Game {
    WINDOW
    fn accepts(&self, conn: u32, packet: S) -> bool { GATE true }
}
#[test]
fn menu_ownership() {
    for owner in 0..7 {
        let mut p = Play::default();
        let mut g = Guest { menu: None };
        let mut menus=Play::default();
        match owner {
            0 => {},
            1 => g.menu = Some(Menu { window: 7 }),
            2 => menus.crafting.table = Some((7,)),
            3 => menus.station = Some(Menu { window: 7 }),
            4 => p.merchant = Some(Menu { window: 7 }),
            5 => p.lectern = Some(Menu { window: 7 }),
            _ => p.mount_window = Some(Menu { window: 7 }),
        }
        let game = Game{lan:Some(LanHost { hub: Hub { g }, play: HashMap::from([(42, p)]),menus:HashMap::from([(42,menus)]) })};
        let expected = if owner == 0 { 0 } else { 7 };
        assert_eq!(game.guest_menu_window(42), expected);
        for requested in [0, 7, 8, -1] {
            for kind in 0..4 {
                let p = Packet { container: requested };
                let packet = match kind { 0 => S::ContainerClick(p), 1 => S::ContainerButtonClick(p),
                    2 => S::ContainerSlotStateChanged(p), _ => S::PlaceRecipe(p) };
                assert_eq!(game.accepts(42, packet), requested == expected,
                           "owner {owner} request {requested} kind {kind}");
            }
        }
        assert!(game.accepts(42, S::Other));
    }
}
'''.replace('WINDOW', window).replace('GATE', gate)
        with tempfile.TemporaryDirectory() as directory:
            src, exe = Path(directory) / 'windows.rs', Path(directory) / 'windows'
            src.write_text(fixture)
            subprocess.run(['rustc', '--edition=2024', '--test', str(src), '-o', str(exe)], check=True)
            subprocess.run([str(exe)], check=True)
