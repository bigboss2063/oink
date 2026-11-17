fn main() {
    let backend = oink_display::platform_backend().expect("backend");
    for d in backend.displays().expect("displays") {
        println!(
            "{} key={} primary={} builtin={} virtual={} bounds=({:.0},{:.0} {:.0}x{:.0}) px={}x{} rot={}",
            d.name, d.key, d.is_primary, d.is_builtin, d.is_virtual,
            d.bounds.x, d.bounds.y, d.bounds.w, d.bounds.h,
            d.pixel_width, d.pixel_height, d.rotation_deg
        );
    }
}
