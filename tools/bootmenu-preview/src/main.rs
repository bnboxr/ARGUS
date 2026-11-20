//! Draw the boot manager screen into host memory and write it out as a PPM.
//!
//! This is the same `argus_shared::bootmenu::paint` the loader calls, run on the
//! build host against a plain 1920x1080 buffer instead of firmware memory, so
//! the layout can be looked at without booting QEMU. It is a development tool:
//! the proof that the screen works in a boot is `tests/bootmenu.sh`, which
//! photographs the real framebuffer.
//!
//!     cargo run -p bootmenu-preview --release -- out.ppm

use std::io::Write;

use argus_shared::bootinfo::{pixel_format, FramebufferInfo};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bootmenu.ppm".to_string());
    let width = 1920u32;
    let height = 1080u32;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let fb = FramebufferInfo {
        phys: pixels.as_mut_ptr() as u64,
        width,
        height,
        stride: width,
        pixel_format: pixel_format::RED_GREEN_BLUE_RESERVED_8BIT,
        size_bytes: pixels.len() as u64,
    };

    match argus_shared::bootmenu::paint(fb) {
        Ok(()) => eprintln!("painted {}x{}", width, height),
        Err(skip) => {
            eprintln!("skipped: {}", skip.reason());
            std::process::exit(1);
        }
    }

    // The surface wrote RGBx8; a PPM wants packed RGB.
    let mut out = Vec::with_capacity((width * height * 3) as usize);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", width, height).as_bytes());
    for chunk in pixels.chunks(4) {
        out.extend_from_slice(&chunk[..3]);
    }
    let mut file = std::fs::File::create(&path).expect("create");
    file.write_all(&out).expect("write");
    eprintln!("wrote {}", path);
}
