//! The spatial index family: the point index, the geometry index, the
//! GeoJSON geometry model they store, and the geodesic/Hilbert maths both
//! walks share. See docs/SPATIAL_FUNCTIONS.md.
pub mod geometry;
pub mod geometry_index;
pub mod math;
pub mod point;
