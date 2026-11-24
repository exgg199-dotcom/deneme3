// game-module-cfg: test
//! The zero-argument server fixture that the tests carried over from Horizon main call
//! (`native_machines::tests::server_game()`). The fork's fixture
//! ([`game_tests::tests::server_game`]) and this wrapper both return an owned game.
//! The temporary resource cache may drop immediately; the Game retains its own Arc.
pub(in crate::game) mod tests {
    use crate::game::Game;

    /// A server's game on the prepared resources: nobody is playing, and the chunk at the origin
    /// is loaded.
    pub(in crate::game) fn server_game() -> Game<'static> {
        let mut resources = None;
        crate::game::game_tests::tests::server_game(&mut resources)
    }
    #[test]
    #[ignore = "daily"]
    fn server_fixture_releases_its_owned_resources() {
        let game = server_game();
        let resources = std::sync::Arc::downgrade(&game.res);
        drop(game);
        assert!(
            resources.upgrade().is_none(),
            "the fixture must not leak its resource owner"
        );
    }
}
