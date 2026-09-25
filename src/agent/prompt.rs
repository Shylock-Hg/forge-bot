//! Prompt text shared by every agent adapter.

use crate::forge::ReplyTarget;

use super::{AgentContext, AgentRequest};

/// Who posts the final reply for this adapter.
#[derive(Clone, Copy)]
pub(super) enum ReplyMode {
    Agent,
    Gateway,
}

pub(super) fn build_prompt(
    request: &AgentRequest,
    context: &AgentContext,
    reply_mode: ReplyMode,
) -> String {
    let mut prompt = String::new();
    match reply_mode {
        ReplyMode::Agent => {
            prompt.push_str("You are an autonomous coding agent invoked from a forge comment.\n\n")
        }
        ReplyMode::Gateway => {
            prompt.push_str("You are the coding agent responding to a forge comment.\n");
            prompt.push_str("Work in the current directory (the repository checkout).\n\n");
        }
    }

    if matches!(reply_mode, ReplyMode::Agent)
        && let Some(forge) = context.forge
    {
        prompt.push_str(&format!("Forge: {forge}\n"));
    }
    prompt.push_str(&format!("Location: {}\n", request.location));
    if !context.repository.is_empty() {
        prompt.push_str(&format!("Repository: {}\n", context.repository));
    }
    if let Some(title) = &context.title {
        prompt.push_str(&format!("Title: {title}\n"));
    }
    let directory_label = match reply_mode {
        ReplyMode::Agent => "Your working directory",
        ReplyMode::Gateway => "Working directory",
    };
    prompt.push_str(&format!(
        "{directory_label}: {}\n\n",
        context.workspace.display()
    ));
    prompt.push_str("Requested work:\n");
    prompt.push_str(request.message.trim());
    prompt.push('\n');

    if !context.requester.is_empty() {
        prompt.push_str(&format!(
            "\nIf you create a pull request, request a review from the caller (@{}) on that pull request.\n",
            context.requester
        ));
    }

    match reply_mode {
        ReplyMode::Agent => {
            if let ReplyTarget::ReviewComment(target) = &context.reply_target {
                prompt.push_str(&format!(
                    "\nThis mention is an inline pull-request review comment. Post any reply in \
                     the same review thread (review id {}, file `{}`, line {}) instead of \
                     opening a new top-level comment.\n",
                    target.review_id, target.path, target.line
                ));
            }
            prompt.push_str(
                "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to \
                 gather context, make changes, run tests, and commit/push when appropriate. \
                 Reply on the forge when you are done. Forge credentials are available in \
                 the environment.\n",
            );
        }
        ReplyMode::Gateway => prompt.push_str(
            "\nWhen finished, reply with a concise summary of what you did and any \
             findings. Do not post to the forge yourself; the gateway relays your \
             final message as the comment reply.\n",
        ),
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ReviewCommentTarget;
    use crate::location::ForgeKind;

    fn sample() -> (AgentRequest, AgentContext) {
        (
            AgentRequest {
                location: "https://forge.example/o/r/pulls/4#issuecomment-9"
                    .parse()
                    .unwrap(),
                message: " fix this ".into(),
            },
            AgentContext {
                forge: Some(ForgeKind::Forgejo),
                repository: "o/r".into(),
                title: Some("A title".into()),
                workspace: "/tmp/ws".into(),
                requester: "alice".into(),
                reply_target: ReplyTarget::ReviewComment(ReviewCommentTarget {
                    review_id: 7,
                    path: "src/lib.rs".into(),
                    line: 42,
                    extra_lines_count: 0,
                }),
                ..Default::default()
            },
        )
    }

    #[test]
    fn command_prompt_matches_the_original_output() {
        let (request, context) = sample();
        assert_eq!(
            build_prompt(&request, &context, ReplyMode::Agent),
            concat!(
                "You are an autonomous coding agent invoked from a forge comment.\n\n",
                "Forge: forgejo\n",
                "Location: https://forge.example/o/r/pulls/4#issuecomment-9\n",
                "Repository: o/r\n",
                "Title: A title\n",
                "Your working directory: /tmp/ws\n\n",
                "Requested work:\nfix this\n",
                "\nIf you create a pull request, request a review from the caller (@alice) on that pull request.\n",
                "\nThis mention is an inline pull-request review comment. Post any reply in the same review thread (review id 7, file `src/lib.rs`, line 42) instead of opening a new top-level comment.\n",
                "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to gather context, make changes, run tests, and commit/push when appropriate. Reply on the forge when you are done. Forge credentials are available in the environment.\n",
            )
        );
    }

    #[test]
    fn pi_rpc_prompt_matches_the_original_output() {
        let (request, context) = sample();
        assert_eq!(
            build_prompt(&request, &context, ReplyMode::Gateway),
            concat!(
                "You are the coding agent responding to a forge comment.\n",
                "Work in the current directory (the repository checkout).\n\n",
                "Location: https://forge.example/o/r/pulls/4#issuecomment-9\n",
                "Repository: o/r\n",
                "Title: A title\n",
                "Working directory: /tmp/ws\n\n",
                "Requested work:\nfix this\n",
                "\nIf you create a pull request, request a review from the caller (@alice) on that pull request.\n",
                "\nWhen finished, reply with a concise summary of what you did and any findings. Do not post to the forge yourself; the gateway relays your final message as the comment reply.\n",
            )
        );
    }

    #[test]
    fn both_reply_modes_share_request_context_and_review_guidance() {
        let request = AgentRequest {
            location: "https://forge.example/o/r/pulls/4".parse().unwrap(),
            message: "fix this".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            requester: "alice".into(),
            reply_target: ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 7,
                path: "src/lib.rs".into(),
                line: 42,
                extra_lines_count: 0,
            }),
            ..Default::default()
        };

        let agent = build_prompt(&request, &context, ReplyMode::Agent);
        let gateway = build_prompt(&request, &context, ReplyMode::Gateway);
        for prompt in [&agent, &gateway] {
            assert!(prompt.contains("Repository: o/r"));
            assert!(prompt.contains("fix this"));
            assert!(prompt.contains("request a review from the caller (@alice)"));
        }
        assert!(agent.contains("review id 7"));
        assert!(agent.contains("Reply on the forge"));
        assert!(!gateway.contains("Reply on the forge"));
        assert!(gateway.contains("gateway relays your"));
    }
}
