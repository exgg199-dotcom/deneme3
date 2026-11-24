"""The actual entity-use dispatch honors the click's secondary-action snapshot."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_mobile_command_keyboard import method

ROOT = Path(__file__).resolve().parents[1]


class EntityInteractionSecondaryTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('rustc'), 'requires Rust')
    def test_click_snapshot_wins_over_newer_movement_input(self):
        source = (ROOT / 'crates/horizon-client/src/game/network/lan_actions/interaction.rs').read_text()
        marker = source.index('g.guest_use = Some(GuestUse::Entity(hand, target));')
        start = source.rfind('.as_guest_at(gi, conn, |g| {', 0, marker)
        start = source.index('{', start) + 1
        body = source[start:source.index('            })', marker)]
        helper = method((ROOT / 'crates/horizon-client/src/game/network/lan_actions/shared.rs').read_text(), 'reset_use_attack_strength')
        fixture = r'''
use std::collections::HashSet;
#[derive(Eq, PartialEq, Hash)] enum KeyCode { ShiftLeft, KeyW }
#[derive(Clone,Copy)] enum GuestUse { Entity((), ()) }
HELPER
struct Game { attack_ticks: u32, keys: HashSet<KeyCode>, guest_use: Option<GuestUse>, swings_started: u32, use_failed:bool, expected: bool }
impl Game {
    fn use_item(&mut self) -> Result<Option<()>, ()> {
        assert_eq!(self.keys.contains(&KeyCode::ShiftLeft), self.expected);
        self.swings_started += 1; Ok(Some(()))
    }
}
#[test]
fn click_snapshot() {
    for movement in [false, true] {
        for secondary in [false, true] {
            let secondary=Some(secondary);
            let expected=secondary.unwrap();
            let (hand, target) = ((), ());
            let mut g = Game { attack_ticks: 9, keys: HashSet::from([KeyCode::KeyW]), guest_use: None,
                               swings_started: 0, use_failed:false, expected };
            if movement { g.keys.insert(KeyCode::ShiftLeft); }
            let (swung,consumed) = (|g: &mut Game| { BODY })(&mut g);
            assert!(swung && consumed && g.guest_use.is_none());
            assert_eq!(g.attack_ticks, 0, "consumed interaction swing resets attack strength");
            assert!(g.keys.contains(&KeyCode::KeyW));
        }
    }
}
'''.replace('BODY', body).replace('HELPER', helper)
        with tempfile.TemporaryDirectory() as directory:
            src, binary = Path(directory) / 'dispatch.rs', Path(directory) / 'dispatch'
            src.write_text(fixture)
            subprocess.run(['rustc', '--edition=2024', '--test', str(src), '-o', str(binary)], check=True)
            subprocess.run([str(binary)], check=True)
