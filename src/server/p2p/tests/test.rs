// Copyright 2024 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

#[cfg(test)]

mod tests {
    use crate::server::p2p::messages::PeerInfo;
    use crate::server::p2p::peer_store::{PeerStore, PeerStoreConfig};
    use libp2p::PeerId;

    #[tokio::test]
    async fn test_add_peer_nominal_case() {
        let store = PeerStore::new(&PeerStoreConfig::default());
        assert_eq!(store.peer_count().await, 0);

        let peer_id = PeerId::random();
        let peer_info = PeerInfo::new(1);

        store.add(peer_id, peer_info).await;
        assert_eq!(store.tip_of_block_height().await.unwrap().height, 1);
        assert_eq!(store.peer_count().await, 1);

        // add four new peers and make sure we the correct max tip height
        for n in (3..=12).step_by(3) {
            let peer_id = PeerId::random();
            let peer_info = PeerInfo::new(n);
            store.add(peer_id, peer_info).await;
        }
        assert_eq!(store.peer_count().await, 5);
        // fails here
        assert_eq!(store.tip_of_block_height().await.unwrap().height, 12);
    }
}
