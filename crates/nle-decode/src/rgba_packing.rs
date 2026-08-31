//! Bounded single-allocation packing for scaled RGBA monitor frames.

use std::sync::Arc;

use super::{MAX_DIMENSION, MAX_FRAME_BYTES};

pub(super) fn pack_rgba_rows(
    data: &[u8],
    stride: usize,
    scaled_size: (u32, u32),
    output_size: (u32, u32),
) -> Result<Arc<[u8]>, String> {
    let (scaled_width, scaled_height) = scaled_size;
    let (output_width, output_height) = output_size;
    validate_dimension("scaled width", scaled_width)?;
    validate_dimension("scaled height", scaled_height)?;
    validate_dimension("output width", output_width)?;
    validate_dimension("output height", output_height)?;
    if scaled_width > output_width || scaled_height > output_height {
        return Err("scaled RGBA frame exceeds its output bounds".to_owned());
    }

    let scaled_width = scaled_width as usize;
    let scaled_height = scaled_height as usize;
    let output_width = output_width as usize;
    let output_height = output_height as usize;
    let source_row_bytes = checked_bytes(scaled_width, "scaled RGBA row")?;
    if stride < source_row_bytes {
        return Err("monitor scaler returned an RGBA stride shorter than its row".to_owned());
    }
    let source_bytes = stride
        .checked_mul(scaled_height)
        .ok_or_else(|| "scaled RGBA source length overflow".to_owned())?;
    if data.len() < source_bytes {
        return Err("monitor scaler returned a truncated RGBA frame".to_owned());
    }

    let output_row_bytes = checked_bytes(output_width, "output RGBA row")?;
    let output_bytes = output_row_bytes
        .checked_mul(output_height)
        .ok_or_else(|| "output RGBA frame length overflow".to_owned())?;
    if output_bytes > MAX_FRAME_BYTES || output_bytes > isize::MAX as usize {
        return Err("output RGBA frame exceeds the allocation limit".to_owned());
    }

    let x_offset = (output_width - scaled_width) / 2;
    let y_offset = (output_height - scaled_height) / 2;
    let mut output = Arc::<[u8]>::new_uninit_slice(output_bytes);
    let destination = Arc::get_mut(&mut output)
        .expect("fresh RGBA allocation must be exclusively owned")
        .as_mut_ptr()
        .cast::<u8>();

    // SAFETY: the validated output allocation has `output_bytes` bytes and remains
    // exclusively owned until `assume_init`. The full allocation is zeroed when
    // letterboxing; otherwise native rows cover every byte. Checked dimensions and
    // lengths bound each source row and destination row, and the fresh allocation
    // cannot overlap `data`.
    unsafe {
        if scaled_size != output_size {
            destination.write_bytes(0, output_bytes);
        }
        for row in 0..scaled_height {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().add(row * stride),
                destination.add((row + y_offset) * output_row_bytes + x_offset * 4),
                source_row_bytes,
            );
        }
        Ok(output.assume_init())
    }
}

fn validate_dimension(name: &str, dimension: u32) -> Result<(), String> {
    if dimension == 0 {
        return Err(format!("{name} must be nonzero"));
    }
    if dimension > MAX_DIMENSION {
        return Err(format!("{name} exceeds the maximum dimension"));
    }
    Ok(())
}

fn checked_bytes(width: usize, label: &str) -> Result<usize, String> {
    width
        .checked_mul(4)
        .ok_or_else(|| format!("{label} byte length overflow"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::pack_rgba_rows;

    #[test]
    fn preserves_exact_native_rgba_rows() {
        let source = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            pack_rgba_rows(&source, 8, (2, 1), (2, 1)).unwrap().as_ref(),
            source
        );
    }

    #[test]
    fn discards_stride_padding_and_keeps_alpha() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 255, 0xEE, 0xEE, 0xEE, 0xEE, 8, 9, 10, 11, 12, 13, 14, 254, 0xDD,
            0xDD, 0xDD, 0xDD,
        ];
        assert_eq!(
            pack_rgba_rows(&source, 12, (2, 2), (2, 2))
                .unwrap()
                .as_ref(),
            &[1, 2, 3, 4, 5, 6, 7, 255, 8, 9, 10, 11, 12, 13, 14, 254]
        );
    }

    #[test]
    fn letterboxes_with_transparent_black_padding() {
        let source = [9, 8, 7, 6, 5, 4, 3, 2];
        let expected = [
            0, 0, 0, 0, 0, 0, 0, 0, 9, 8, 7, 6, 5, 4, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            pack_rgba_rows(&source, 8, (2, 1), (2, 3)).unwrap().as_ref(),
            expected
        );
    }

    #[test]
    fn odd_letterbox_extra_pixels_are_right_and_bottom() {
        let source = [11, 12, 13, 14];
        let packed = pack_rgba_rows(&source, 4, (1, 1), (4, 4)).unwrap();
        let pixel = |x: usize, y: usize| &packed[(y * 4 + x) * 4..(y * 4 + x + 1) * 4];
        assert_eq!(pixel(1, 1), source);
        assert_eq!(pixel(0, 1), [0; 4]);
        assert_eq!(pixel(2, 1), [0; 4]);
        assert_eq!(pixel(3, 1), [0; 4]);
        assert_eq!(pixel(1, 0), [0; 4]);
        assert_eq!(pixel(1, 2), [0; 4]);
        assert_eq!(pixel(1, 3), [0; 4]);
    }

    #[test]
    fn one_pixel_frame_is_packed() {
        assert_eq!(
            pack_rgba_rows(&[0, 1, 2, 3], 4, (1, 1), (1, 1))
                .unwrap()
                .as_ref(),
            &[0, 1, 2, 3]
        );
    }

    #[test]
    fn rejects_malformed_inputs() {
        for (data, stride, scaled, output) in [
            (&[][..], 0, (0, 1), (1, 1)),
            (&[0; 4][..], 4, (1, 1), (0, 1)),
            (&[0; 4][..], 4, (4_097, 1), (4_097, 1)),
            (&[0; 4][..], 4, (2, 1), (1, 1)),
            (&[0; 7][..], 7, (2, 1), (2, 1)),
            (&[0; 7][..], 8, (2, 1), (2, 1)),
            (&[0; 8][..], usize::MAX, (1, 2), (1, 2)),
        ] {
            assert!(pack_rgba_rows(data, stride, scaled, output).is_err());
        }
    }

    #[test]
    fn output_owns_its_bytes_and_arc_clones_share_them() {
        let packed = {
            let source = vec![1, 2, 3, 4];
            pack_rgba_rows(&source, 4, (1, 1), (1, 1)).unwrap()
        };
        let clone = Arc::clone(&packed);
        assert!(Arc::ptr_eq(&packed, &clone));
        assert_eq!(clone.as_ref(), &[1, 2, 3, 4]);
    }
}
