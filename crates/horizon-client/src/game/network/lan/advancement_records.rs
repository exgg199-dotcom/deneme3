use super::*;

impl Game<'_> {
    /// `PlayerAdvancements.save`: the existing network player's advancement file.
    pub(in crate::game) fn save_guest_advancements(
        &self,
        host: &LanHost,
        guest: &horizon_client::lan::guest::Guest,
    ) -> horizon_client::Result<()> {
        if !guest.placed
            || guest.released
            || !guest.participates_in_player_simulation()
            || self.adv.tree.list.is_empty()
        {
            return Ok(());
        }
        let Some(uuid) = self.records_uuid(host, guest.uuid) else {
            return Ok(());
        };
        let path = super::stats::player_json(&self.world_root, "advancements", uuid);
        let json = guest
            .advancements
            .to_json(&self.adv.tree, horizon_world::DATA_VERSION);
        // A failed write must leave the previous advancement file readable.
        super::stats::write_player_json(&path, &json)
    }
    /// `PlayerAdvancements.save` and `ServerStatsCounter.save` of a network player.
    pub(in crate::game) fn save_guest_records(
        &self,
        host: &LanHost,
        guest: &horizon_client::lan::guest::Guest,
    ) -> horizon_client::Result<()> {
        if !guest.placed || guest.released || !guest.participates_in_player_simulation() {
            return Ok(());
        }
        let Some(uuid) = self.records_uuid(host, guest.uuid) else {
            return Ok(());
        };
        let advancements = self.save_guest_advancements(host, guest);
        let stats = host.play.get(&guest.conn).map_or(Ok(()), |play| {
            let path = super::stats::player_json(&self.world_root, "stats", uuid);
            super::stats::write_player_json(&path, &play.stats.to_json())
        });
        advancements.and(stats)
    }
}
