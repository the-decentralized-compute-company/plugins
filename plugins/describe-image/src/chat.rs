//! Building a chat completions request that carries pictures, and reading the
//! answer back.
//!
//! # The content format
//!
//! A message that carries an image is not a string; it is an array of typed
//! parts, and the image part wraps a URL rather than raw bytes:
//!
//! ```jsonc
//! { "role": "user", "content": [
//!     { "type": "text", "text": "Describe this image." },
//!     { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,/9j/4AAQ…" } }
//! ] }
//! ```
//!
//! The `data:` URI form is used rather than an `https://` one on purpose: an
//! `https://` URL would have to be fetched *by the inference server*, which
//! means the picture is fetched from a machine and a network position this
//! plugin does not control and cannot reason about. Inlining the bytes keeps
//! the whole transfer inside the request this node made.
//!
//! There is deliberately **no system message**. Multimodal chat templates vary
//! in whether they accept one — several merge it into the first user turn, some
//! reject it outright — and the instruction works just as well as the leading
//! text part, which every template handles. `detail` is likewise left off the
//! image part: it is an OpenAI-hosted concept, ignored by llama.cpp and vLLM,
//! and this plugin does not advertise settings it cannot honour.

use serde_json::{Value, json};

/// Fixed sampling temperature per task.
///
/// Not a configuration knob, because the right answer is not an operator
/// preference. A transcription has one correct output and any sampling spread
/// is corruption; a description is a summary, where a little spread reads as
/// fluency rather than as error.
pub const DESCRIBE_TEMPERATURE: f64 = 0.2;
pub const READ_TEXT_TEMPERATURE: f64 = 0.0;

/// The exact string `read_text` asks a model to emit when it finds nothing, so
/// "no text" is distinguishable from "the model refused to answer".
pub const NO_TEXT_SENTINEL: &str = "(no legible text)";

/// The instruction that leads the message, for a plain description.
pub fn describe_instruction(image_count: usize, focus: Option<&str>) -> String {
    let mut instruction = String::from(
        "Describe what is visible in the image in plain prose. Cover the main subject, the \
         setting, and any text, labels, or numbers that are legible. Report only what you can \
         actually see: if a detail is unclear, say that it is unclear rather than guessing.",
    );
    if let Some(focus) = focus.map(str::trim).filter(|focus| !focus.is_empty()) {
        instruction.push_str(&format!(" Pay particular attention to: {focus}"));
        if !focus.ends_with('.') {
            instruction.push('.');
        }
    }
    prefix_for_count(image_count) + &instruction
}

/// The instruction that leads the message, for a question.
pub fn ask_instruction(image_count: usize, question: &str) -> String {
    format!(
        "{}Answer this question about the image using only what is visible in it: {}\n\nIf the \
         image does not contain enough information to answer, say so plainly instead of \
         speculating.",
        prefix_for_count(image_count),
        question.trim()
    )
}

/// The instruction that leads the message, for text extraction.
pub fn read_text_instruction(image_count: usize) -> String {
    format!(
        "{}Transcribe every piece of text visible in the image. Preserve the reading order and \
         the line breaks. Do not translate, summarise, correct, or explain anything — output the \
         transcribed text and nothing else. Where a character is genuinely illegible, write [?] \
         in its place. If there is no legible text at all, reply with exactly: {NO_TEXT_SENTINEL}",
        prefix_for_count(image_count)
    )
}

/// A leading sentence that numbers the images, so a multi-image answer can
/// refer to them unambiguously.
fn prefix_for_count(image_count: usize) -> String {
    if image_count <= 1 {
        String::new()
    } else {
        format!(
            "There are {image_count} images, in order; refer to them as image 1 through image \
             {image_count}. "
        )
    }
}

/// Build the request body.
///
/// `image_data_uris` are already downscaled and encoded; this function does not
/// look at bytes at all, which is what makes it testable without an image.
pub fn build_request(
    model: &str,
    instruction: &str,
    image_data_uris: &[String],
    max_tokens: u32,
    temperature: f64,
) -> Value {
    let mut content = vec![json!({ "type": "text", "text": instruction })];
    for uri in image_data_uris {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": uri },
        }));
    }

    json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": temperature,
        // Streaming would buy nothing here: there is one message and the host
        // hands a tool result back whole.
        "stream": false,
        "messages": [{ "role": "user", "content": content }],
    })
}

/// What came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// Pull the answer out of an OpenAI-shaped chat completion.
///
/// Handles both content shapes servers actually emit: a plain string, and the
/// array-of-parts form some multimodal servers echo back. An empty answer is an
/// error rather than an empty success — a caller looking at `""` cannot tell a
/// model that saw nothing from a request that was truncated before it produced
/// a token.
pub fn parse_completion(body: &str) -> Result<Completion, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("the completion response is not JSON ({error})"))?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return Err(format!(
            "the inference endpoint returned an error: {message}"
        ));
    }

    let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
        return Err("the completion response has no choices[0] entry".to_string());
    };
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);

    let content = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .ok_or_else(|| "the completion response has no choices[0].message.content".to_string())?;
    let text = flatten_content(content).trim().to_string();

    let usage = value.get("usage");
    let usage_field = |name: &str| {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
    };

    if text.is_empty() {
        return Err(match finish_reason.as_deref() {
            Some("length") => format!(
                "the model produced no text before hitting the {} token budget. Raise \
                 --max-tokens, or the `max_tokens` argument on this call.",
                usage_field("completion_tokens")
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "output".to_string())
            ),
            Some(reason) => format!(
                "the model returned an empty message (finish_reason `{reason}`). That usually \
                 means it could not accept the image: check that the selected model really is \
                 vision-capable and that its projector file is loaded."
            ),
            None => "the model returned an empty message.".to_string(),
        });
    }

    Ok(Completion {
        text,
        finish_reason,
        prompt_tokens: usage_field("prompt_tokens"),
        completion_tokens: usage_field("completion_tokens"),
    })
}

/// Reduce a content value to text, whichever of the two shapes it is in.
fn flatten_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(_) => part.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<String>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_puts_the_instruction_first_and_every_image_after_it() {
        let request = build_request(
            "qwen3-vl",
            "Describe this image.",
            &[
                "data:image/jpeg;base64,AAAA".to_string(),
                "data:image/png;base64,BBBB".to_string(),
            ],
            256,
            0.2,
        );

        assert_eq!(request["model"], "qwen3-vl");
        assert_eq!(request["max_tokens"], 256);
        assert_eq!(request["stream"], false);
        assert_eq!(request["messages"][0]["role"], "user");

        let content = request["messages"][0]["content"]
            .as_array()
            .expect("content is an array of parts");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Describe this image.");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
        assert_eq!(content[2]["image_url"]["url"], "data:image/png;base64,BBBB");
    }

    #[test]
    fn no_system_message_and_no_detail_field_are_emitted() {
        let request = build_request(
            "m",
            "hi",
            &["data:image/png;base64,AA".to_string()],
            64,
            0.0,
        );

        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1, "a single user turn, by design");
        let image_part = &request["messages"][0]["content"][1]["image_url"];
        assert!(
            image_part.get("detail").is_none(),
            "`detail` is an OpenAI-hosted concept that local servers ignore"
        );
    }

    #[test]
    fn a_request_with_no_images_is_still_well_formed() {
        // Never reached through a tool — the arg validation requires at least
        // one image — but the builder must not produce something malformed.
        let request = build_request("m", "hi", &[], 64, 0.0);
        assert_eq!(
            request["messages"][0]["content"]
                .as_array()
                .expect("array")
                .len(),
            1
        );
    }

    #[test]
    fn a_single_image_instruction_carries_no_numbering_preamble() {
        let instruction = describe_instruction(1, None);
        assert!(!instruction.contains("image 1"), "{instruction}");
        assert!(instruction.starts_with("Describe what is visible"));
    }

    #[test]
    fn several_images_are_numbered_so_an_answer_can_refer_to_them() {
        for instruction in [
            describe_instruction(3, None),
            ask_instruction(3, "which one is brightest?"),
            read_text_instruction(3),
        ] {
            assert!(instruction.contains("3 images"), "{instruction}");
            assert!(
                instruction.contains("image 1 through image 3"),
                "{instruction}"
            );
        }
    }

    #[test]
    fn a_focus_is_appended_and_punctuated() {
        let instruction = describe_instruction(1, Some("the serial number on the label"));
        assert!(
            instruction.ends_with("Pay particular attention to: the serial number on the label."),
            "{instruction}"
        );

        // An empty or whitespace focus changes nothing.
        assert_eq!(
            describe_instruction(1, Some("   ")),
            describe_instruction(1, None)
        );
        assert_eq!(describe_instruction(1, None), describe_instruction(1, None));
    }

    #[test]
    fn every_instruction_tells_the_model_not_to_invent() {
        assert!(describe_instruction(1, None).contains("only what you can actually see"));
        assert!(ask_instruction(1, "q").contains("instead of speculating"));
        assert!(read_text_instruction(1).contains("Do not translate"));
    }

    #[test]
    fn the_question_is_trimmed_into_the_instruction() {
        let instruction = ask_instruction(1, "  How many people are there?  ");
        assert!(
            instruction.contains("How many people are there?"),
            "{instruction}"
        );
        assert!(!instruction.contains("  How"), "{instruction}");
    }

    #[test]
    fn read_text_names_the_sentinel_it_wants_for_an_empty_result() {
        assert!(read_text_instruction(1).contains(NO_TEXT_SENTINEL));
    }

    #[test]
    fn a_normal_completion_parses_with_its_usage() {
        let body = r#"{"id":"x","choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"A tabby cat on a windowsill."}}],
            "usage":{"prompt_tokens":812,"completion_tokens":9,"total_tokens":821}}"#;

        let completion = parse_completion(body).expect("parses");
        assert_eq!(completion.text, "A tabby cat on a windowsill.");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert_eq!(completion.prompt_tokens, Some(812));
        assert_eq!(completion.completion_tokens, Some(9));
    }

    #[test]
    fn the_array_of_parts_content_shape_is_flattened() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":[
            {"type":"text","text":"A tabby cat"},{"type":"text","text":" on a windowsill."}]}}]}"#;

        let completion = parse_completion(body).expect("parses");
        assert_eq!(completion.text, "A tabby cat on a windowsill.");
    }

    #[test]
    fn an_empty_answer_is_an_error_and_names_the_likely_cause() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":""}}]}"#;
        let error = parse_completion(body).expect_err("empty is not success");
        assert!(error.contains("vision-capable"), "{error}");
        assert!(error.contains("projector"), "{error}");
    }

    #[test]
    fn an_answer_truncated_before_it_started_points_at_the_token_budget() {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"content":"   "}}],
            "usage":{"completion_tokens":64}}"#;
        let error = parse_completion(body).expect_err("empty is not success");
        assert!(error.contains("--max-tokens"), "{error}");
        assert!(error.contains("64"), "{error}");
    }

    #[test]
    fn an_error_body_is_quoted_rather_than_reported_as_a_shape_problem() {
        let body = r#"{"error":{"message":"model does not support images","code":400}}"#;
        let error = parse_completion(body).expect_err("must fail");
        assert!(error.contains("model does not support images"), "{error}");
    }

    #[test]
    fn every_malformed_completion_shape_says_what_is_missing() {
        let cases = [
            ("not json", "not JSON"),
            (r#"{"choices":[]}"#, "choices[0]"),
            (r#"{"choices":[{"message":{}}]}"#, "message.content"),
        ];
        for (body, expected) in cases {
            let error = parse_completion(body).expect_err("must fail");
            assert!(error.contains(expected), "{body} -> {error}");
        }
    }

    #[test]
    fn a_completion_without_usage_still_parses() {
        let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let completion = parse_completion(body).expect("parses");
        assert_eq!(completion.prompt_tokens, None);
        assert_eq!(completion.finish_reason, None);
    }
}
