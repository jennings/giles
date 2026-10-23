//! `load_skill` and `send_attachment` through [`CoreTool::call`], against a temporary skill
//! directory, blob store, and a fake channel.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{BlobMeta, BlobStore, Session, SessionId, SessionKind, Store};
use giles_types::{
    AgentId, Block, CallContext, Channel, ChannelEvent, ChannelInfo, ConversationId, EventId,
    MediaId, MediaRef, Message, MessageId, MessageMeta, OutboundMessage, OutputMode, Result, Role,
    SendTarget, Timestamp,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::identity::owner;
use crate::media::{self, SessionMedia};
use crate::sessions::{ChannelLookup, Reply};
use crate::skills::{SkillSource, discover};

/// Records what it's sent.
struct FakeChannel {
    info: ChannelInfo,
    sent: Mutex<Vec<OutboundMessage>>,
}

impl Default for FakeChannel {
    fn default() -> Self {
        Self {
            info: ChannelInfo {
                identity_namespace: "fake".to_owned(),
                output: OutputMode::Blocks,
                can_initiate: false,
            },
            sent: Mutex::default(),
        }
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(stream::pending().boxed())
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.sent.lock().unwrap().push(msg);
        Ok(MessageId::new("sent").unwrap())
    }
}

/// Only instance `fake` has a channel.
struct Lookup(Arc<FakeChannel>);

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (instance == "fake").then(|| self.0.clone() as Arc<dyn Channel>)
    }
}

struct Setup {
    dir: TempDir,
    blobs: BlobStore,
    channel: Arc<FakeChannel>,
}

impl Setup {
    /// A skill `demo` (with files, a hidden one, and a link out) and a secret file beside it.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("skills/demo");
        let write = |path: PathBuf, contents: &[u8]| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            skill.join("SKILL.md"),
            b"---\nname: demo\ndescription: Demo.\n---\nRun `scripts/run.py`.\n",
        );
        write(skill.join("scripts/run.py"), b"print('hi')\n");
        write(skill.join("logo.png"), b"\x89PNG\r\n\x1a\n\x00");
        write(skill.join(".env"), b"TOKEN=x");
        write(dir.path().join("secret.txt"), b"the secret");
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), skill.join("escape.txt"))
            .unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), store)
            .await
            .unwrap();
        Self {
            dir,
            blobs,
            channel: Arc::default(),
        }
    }

    fn skills(&self) -> Arc<crate::skills::SkillSet> {
        Arc::new(discover(&[SkillSource::local(self.dir.path().join("skills"))]).skills)
    }

    fn session(&self, kind: SessionKind, channel: &str) -> Session {
        let job = kind == SessionKind::Job;
        Session {
            id: SessionId::new(1),
            agent: AgentId::new("jeeves").unwrap(),
            kind,
            channel: (!job).then(|| channel.to_owned()),
            conversation: (!job).then(|| ConversationId::new("owner").unwrap()),
            user: Some(owner()),
            started_at: Timestamp::UNIX_EPOCH,
            ended_at: None,
            last_message_at: None,
        }
    }

    /// Calls `tool` in a turn of `session` whose history holds `media`.
    async fn call(
        &self,
        tool: &dyn CoreTool,
        input: Value,
        session: Session,
        media: &[MediaRef],
    ) -> ToolOutput {
        self.call_listing(tool, input, session, media, self.skills())
            .await
    }

    /// As [`call`](Self::call), in a session whose prompt lists `skills`.
    async fn call_listing(
        &self,
        tool: &dyn CoreTool,
        input: Value,
        session: Session,
        media: &[MediaRef],
        skills: Arc<crate::skills::SkillSet>,
    ) -> ToolOutput {
        let reply = Reply::for_session(&session, Arc::new(Lookup(self.channel.clone())));
        let history = Message {
            role: Role::User,
            content: media.iter().cloned().map(media::block).collect(),
            meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
        };
        tool.call(CoreCall {
            id: "t1".to_owned(),
            input,
            context: CallContext {
                agent: session.agent.clone(),
                requester: Some(owner()),
                conversation: None,
            },
            turn: Arc::new(TurnContext {
                session,
                reply,
                skills,
            }),
            media: Arc::new(SessionMedia::from_messages(&[history])),
        })
        .await
    }

    async fn load(&self, input: Value) -> ToolOutput {
        let tool = LoadSkillTool::new(self.blobs.clone());
        let session = self.session(SessionKind::Direct, "fake");
        self.call(&tool, input, session, &[]).await
    }

    async fn put(&self, bytes: &'static [u8], media_type: &str) -> MediaRef {
        let meta = BlobMeta {
            media_type: media_type.to_owned(),
            name: Some("photo.png".to_owned()),
        };
        self.blobs
            .put(bytes, 1024, meta, Timestamp::UNIX_EPOCH)
            .await
            .unwrap()
    }
}

fn error(output: &ToolOutput) -> &str {
    assert!(output.is_error, "{output:?}");
    match output.content.as_slice() {
        [Block::Text { text }] => text,
        other => panic!("not one text block: {other:?}"),
    }
}

#[tokio::test]
async fn load_skill_gives_the_body_and_the_visible_files() {
    let setup = Setup::new().await;
    let output = setup.load(json!({"name": "demo"})).await;
    assert!(!output.is_error);
    assert_eq!(
        output.content,
        [Block::text(
            "Run `scripts/run.py`.\n\nFiles of this skill (read one with `load_skill` and \
             `file`):\n- logo.png\n- scripts/run.py"
        )]
    );
}

#[tokio::test]
async fn load_skill_refuses_skills_the_session_does_not_list() {
    let setup = Setup::new().await;
    for name in ["nope", "../demo", "demo/..", ""] {
        let output = setup.load(json!({"name": name})).await;
        assert_eq!(error(&output), format!("unknown skill `{name}`"));
    }
    let output = setup.load(json!({"skill": "demo"})).await;
    assert!(error(&output).starts_with("invalid input:"), "{output:?}");

    // `demo` exists on disk, but this session's prompt didn't list it (no `load_skill` when it
    // started, or the skill appeared later): it can't be loaded.
    let tool = LoadSkillTool::new(setup.blobs.clone());
    let session = setup.session(SessionKind::Direct, "fake");
    let output = setup
        .call_listing(&tool, json!({"name": "demo"}), session, &[], Arc::default())
        .await;
    assert_eq!(error(&output), "unknown skill `demo`");
}

#[tokio::test]
async fn load_skill_hands_a_file_over_as_text_and_media() {
    let setup = Setup::new().await;
    let output = setup
        .load(json!({"name": "demo", "file": "scripts/run.py"}))
        .await;
    assert!(!output.is_error, "{output:?}");
    let [
        Block::Text { text },
        reference,
        Block::Document { media, name },
    ] = output.content.as_slice()
    else {
        panic!("{output:?}");
    };
    assert_eq!(
        text,
        "`scripts/run.py` (text/x-python, 12 bytes):\n\nprint('hi')\n"
    );
    assert_eq!(media.media_type, "text/x-python");
    assert_eq!(name.as_deref(), Some("run.py"));
    assert_eq!(*reference, Block::text(media::reference(media)));
    let stored = setup.blobs.stat(&media.id).await.unwrap().unwrap();
    assert_eq!(stored.size, 12);

    // A binary file is only attached, as its media type's block.
    let output = setup
        .load(json!({"name": "demo", "file": "logo.png"}))
        .await;
    assert!(matches!(
        output.content.as_slice(),
        [Block::Text { text }, Block::Text { .. }, Block::Image { .. }]
            if text == "`logo.png` (image/png, 9 bytes) is attached."
    ));
}

#[tokio::test]
async fn load_skill_never_reads_outside_the_skill() {
    let setup = Setup::new().await;
    let secret = setup.dir.path().join("secret.txt");
    for file in [
        "../secret.txt",
        "../../secret.txt",
        "scripts/../../secret.txt",
        secret.to_str().unwrap(),
        "escape.txt",
        ".env",
        "missing.txt",
    ] {
        let output = setup.load(json!({"name": "demo", "file": file})).await;
        assert_eq!(error(&output), format!("the skill has no file `{file}`"));
    }
    // Nothing was stored.
    let secret_id = MediaId::from_digest(sha256(b"the secret"));
    assert_eq!(setup.blobs.stat(&secret_id).await.unwrap(), None);
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

#[tokio::test]
async fn send_attachment_sends_session_media_to_the_conversation() {
    let setup = Setup::new().await;
    let photo = setup.put(b"png", "image/png").await;
    let tool = SendAttachmentTool::new(setup.blobs.clone());
    let session = setup.session(SessionKind::Direct, "fake");
    let output = setup
        .call(
            &tool,
            json!({"media": photo.id.as_str(), "caption": "The **cat**."}),
            session,
            std::slice::from_ref(&photo),
        )
        .await;
    assert_eq!(
        output.content,
        [Block::text(format!("Sent {}.", media::reference(&photo)))]
    );
    let sent = setup.channel.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].target,
        SendTarget::Conversation(ConversationId::new("owner").unwrap())
    );
    assert_eq!(sent[0].markdown, "The **cat**.");
    assert_eq!(sent[0].attachments, [photo]);
}

#[tokio::test]
async fn send_attachment_refuses_what_it_cannot_send() {
    let setup = Setup::new().await;
    let photo = setup.put(b"png", "image/png").await;
    let tool = SendAttachmentTool::new(setup.blobs.clone());
    let direct = || setup.session(SessionKind::Direct, "fake");
    let input = json!({"media": photo.id.as_str()});
    let session_media = std::slice::from_ref(&photo);

    // Stored, but not in the session's history.
    let output = setup.call(&tool, input.clone(), direct(), &[]).await;
    assert_eq!(error(&output), format!("unknown media `{}`", photo.id));
    let output = setup
        .call(&tool, json!({"media": "nope"}), direct(), session_media)
        .await;
    assert_eq!(error(&output), "unknown media `nope`");

    // A job run has no conversation.
    let job = setup.session(SessionKind::Job, "fake");
    let output = setup.call(&tool, input.clone(), job, session_media).await;
    assert_eq!(
        error(&output),
        "this session has no conversation to send to"
    );

    // The conversation's channel isn't registered.
    let gone = setup.session(SessionKind::Direct, "irc");
    let output = setup.call(&tool, input.clone(), gone, session_media).await;
    assert_eq!(
        error(&output),
        "sending failed: unavailable: channel `irc` is not available yet"
    );

    // In the session's history, but no longer stored.
    let swept = MediaRef {
        id: MediaId::from_digest([7; 32]),
        ..photo.clone()
    };
    let output = setup
        .call(
            &tool,
            json!({"media": swept.id.as_str()}),
            direct(),
            std::slice::from_ref(&swept),
        )
        .await;
    assert_eq!(
        error(&output),
        format!("media `{}` is no longer stored", swept.id)
    );
    assert!(setup.channel.sent.lock().unwrap().is_empty());
}
