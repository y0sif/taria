//! Test-only helpers shared by the unit-test modules.

use std::ops::{Deref, DerefMut};
use std::os::unix::fs::PermissionsExt;

use crate::TariaLayer;

/// A [`TariaLayer`] bound in its own temporary directory.
///
/// Dropping the guard drops the layer first (which unlinks the socket file)
/// and then removes the directory itself - also when the test panics - so
/// test runs leave nothing behind in the system temp dir.
pub(crate) struct TestLayer {
    layer: TariaLayer,
    _dir: tempfile::TempDir,
}

/// Bind a layer for `label` on a socket in a fresh self-cleaning temp dir.
/// `prefix` names the temp dir so leftovers (which should never exist) can
/// be traced back to the module that leaked them.
pub(crate) fn bind_test_layer(prefix: &str, label: &str) -> TestLayer {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        // The layer vets the socket dir: it must be private (0700).
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("create test socket dir");
    let layer = TariaLayer::bind_at(label, dir.path().join(format!("{label}.sock")))
        .expect("bind test layer");
    TestLayer { layer, _dir: dir }
}

impl Deref for TestLayer {
    type Target = TariaLayer;

    fn deref(&self) -> &TariaLayer {
        &self.layer
    }
}

impl DerefMut for TestLayer {
    fn deref_mut(&mut self) -> &mut TariaLayer {
        &mut self.layer
    }
}
