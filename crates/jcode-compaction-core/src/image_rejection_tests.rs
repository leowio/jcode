use super::*;

#[test]
fn codex_lb_truncated_input_image_triggers_recovery_with_or_without_envelope() {
    let message = "input_image image_url could not parse image: unexpected end of file";
    assert!(is_image_rejection_error(message));
    assert!(is_image_rejection_error(&format!(
        "HTTP 400 invalid_request_error: {message}"
    )));
    assert!(!is_image_rejection_error("unexpected end of file"));
    assert!(!is_image_rejection_error(
        "HTTP 400: invalid_request_error: missing model"
    ));
}

#[test]
fn openai_invalid_image_data_triggers_recovery() {
    assert!(is_image_rejection_error(
        "invalid_request_error (invalid_value): The image data you provided does not represent a valid image. Please check your input and try again."
    ));
    assert!(is_image_rejection_error(
        "HTTP 400: The image data you provided does not represent a valid image."
    ));
    assert!(!is_image_rejection_error(
        "The image data you provided does not represent a valid image."
    ));
    assert!(!is_image_rejection_error(
        "503: The image data you provided does not represent a valid image."
    ));
}

#[test]
fn strip_all_images_replaces_every_image_with_a_note() {
    let mut a = vec![
        ContentBlock::Text {
            text: "keep".into(),
            cache_control: None,
        },
        ContentBlock::Image {
            media_type: "image/bmp".into(),
            data: "Qk0=".into(),
        },
    ];
    let mut b = vec![ContentBlock::Image {
        media_type: "image/png".into(),
        data: "iVBO".into(),
    }];
    let mut contents = vec![&mut a, &mut b];
    assert_eq!(strip_all_images_in_contents(&mut contents), 2);
    assert!(matches!(&a[0], ContentBlock::Text { text, .. } if text == "keep"));
    assert!(
        matches!(&a[1], ContentBlock::Text { text, .. } if text.contains("media_type=image/bmp"))
    );
    assert!(matches!(&b[0], ContentBlock::Text { .. }));
}
