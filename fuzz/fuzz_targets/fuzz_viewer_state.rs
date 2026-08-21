#![no_main]

use dustnet::OperationOwner;
use dustnet::viewer::{ViewerEffect, ViewerEvent, ViewerModel};
use dustnet_core::protocol::origin::{Origin, TransportSecurity};
use dustnet_core::protocol::uri::AtpUri;
use libfuzzer_sys::fuzz_target;

fn target(name: &str) -> (AtpUri, Origin) {
    let uri = AtpUri::parse(&format!("atp://{name}.example/")).unwrap();
    let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
    (uri, origin)
}

fuzz_target!(|data: &[u8]| {
    let mut viewer = ViewerModel::new(80, 24);
    let mut completion: Option<OperationOwner> = None;
    for event in data.chunks_exact(4) {
        let effects = match event[0] % 9 {
            0 => {
                let (uri, origin) = target(if event[1] & 1 == 0 { "one" } else { "two" });
                viewer.reduce(ViewerEvent::Navigate { uri, origin })
            }
            1 => viewer.reduce(ViewerEvent::Resize {
                width: u16::from(event[1]).saturating_add(1),
                height: u16::from(event[2]).saturating_add(1),
            }),
            2 => completion.clone().map_or_else(Vec::new, |owner| {
                viewer.reduce(ViewerEvent::Connected { owner })
            }),
            3 => completion.clone().map_or_else(Vec::new, |owner| {
                viewer.reduce(ViewerEvent::FetchCompleted { owner })
            }),
            4 => completion.clone().map_or_else(Vec::new, |owner| {
                viewer.reduce(ViewerEvent::ParseCompleted { owner })
            }),
            5 => completion.clone().map_or_else(Vec::new, |owner| {
                viewer.reduce(ViewerEvent::LayoutPrepared {
                    owner,
                    content_height: u32::from(event[3]) * 32,
                })
            }),
            6 => viewer.reduce(ViewerEvent::Timer),
            7 => viewer
                .scope()
                .map(|scope| scope.origin().clone())
                .map_or_else(Vec::new, |origin| {
                    viewer.reduce(ViewerEvent::TransportLost { origin })
                }),
            _ => viewer.control_token().map_or_else(Vec::new, |token| {
                viewer.reduce(ViewerEvent::Input {
                    token,
                    value: String::from_utf8_lossy(event).into_owned(),
                })
            }),
        };
        for effect in effects {
            if let ViewerEffect::Connect { owner } | ViewerEffect::Fetch { owner, .. } = effect
            {
                completion = Some(owner);
            }
        }
        assert!(viewer.viewport().0 > 0 && viewer.viewport().1 > 0);
    }
    let _ = viewer.reduce(ViewerEvent::Shutdown);
});
