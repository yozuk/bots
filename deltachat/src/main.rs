use anyhow::Result;
use clap::Parser;
use deltachat::chat::{self, ChatId};
use deltachat::config;
use deltachat::contact::Contact;
use deltachat::context::*;
use deltachat::message::{Message, Viewtype};
use deltachat::{Event, EventType, Events};
use std::io::Write;
use std::path::PathBuf;
use std::str;
use tempfile::NamedTempFile;
use yozuk::Yozuk;
use yozuk_sdk::prelude::*;

#[derive(Parser)]
#[clap(author, version, about)]
pub struct Args {
    #[clap(long)]
    pub mail: String,

    #[clap(long, env("MAIL_PASSWORD"), hide_env_values = true)]
    pub password: String,

    #[clap(long)]
    pub dbfile: PathBuf,
}

const DECRYPT_FAILURE: &str = "[This message was encrypted for another setup.]";

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::try_parse()?;
    pretty_env_logger::try_init_timed()?;

    let ctx = Context::new(&args.dbfile, 0, Events::new()).await?;
    let server = Server::new(args, ctx)?;
    server.start().await?;
    Ok(())
}

struct Server {
    args: Args,
    ctx: Context,
    zuk: Yozuk,
}

impl Server {
    fn new(args: Args, ctx: Context) -> Result<Self> {
        let zuk = Yozuk::builder().build();
        Ok(Self { args, ctx, zuk })
    }

    async fn start(&self) -> Result<()> {
        let mut avatar = NamedTempFile::new()?;
        avatar.write_all(include_bytes!("yozuk.png"))?;
        avatar.flush()?;
        let avatar = avatar.into_temp_path();

        log::info!("configuring");
        self.ctx
            .set_config(config::Config::Addr, Some(&self.args.mail))
            .await?;
        self.ctx
            .set_config(config::Config::MailPw, Some(&self.args.password))
            .await?;
        self.ctx
            .set_config(config::Config::Displayname, Some("Yozuk"))
            .await?;
        self.ctx
            .set_config(config::Config::Selfavatar, avatar.to_str())
            .await?;
        self.ctx.configure().await?;
        self.ctx.start_io().await;

        let events = self.ctx.get_event_emitter();
        while let Some(event) = events.recv().await {
            self.handle_event(event).await?;
        }

        Ok(())
    }

    async fn handle_event(&self, event: Event) -> Result<()> {
        if let EventType::IncomingMsg { msg_id, .. } = event.typ {
            deltachat::message::markseen_msgs(&self.ctx, vec![msg_id]).await?;
            let msg = Message::load_from_db(&self.ctx, msg_id).await?;
            let chat_id = ChatId::create_for_contact(&self.ctx, msg.get_from_id()).await?;
            let contact = Contact::load_from_db(&self.ctx, msg.get_from_id()).await?;
            let username = if contact.get_addr() == contact.get_display_name() {
                None
            } else {
                Some(contact.get_display_name().to_string())
            };
            let user = UserContext {
                username,
                ..Default::default()
            };

            if let Some(text) = msg.get_text() {
                if text.ends_with(DECRYPT_FAILURE) {
                    chat::send_text_msg(
                        &self.ctx,
                        chat_id,
                        "Sorry, I can't decrypt your message. Please send the message again."
                            .into(),
                    )
                    .await?;
                } else if !msg.is_system_message() {
                    let mut streams = vec![];
                    if let Some(file) = msg.get_file(&self.ctx) {
                        let data = deltachat::tools::open_file_std(&self.ctx, file)?;
                        let media_type = msg
                            .get_filemime()
                            .and_then(|mime| MediaTypeBuf::from_string(mime).ok())
                            .unwrap_or_else(|| media_type!(APPLICATION / OCTET_STREAM).into());
                        streams.push(InputStream::new(data, media_type));
                    }
                    self.handle_command(chat_id, text, streams, user).await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_command(
        &self,
        chat_id: ChatId,
        text: String,
        mut streams: Vec<InputStream>,
        user: UserContext,
    ) -> Result<()> {
        let tokenizer = Tokenizer::new();
        let tokens = tokenizer.tokenize(&text);
        let commands = self.zuk.get_commands(&tokens, &streams);
        if commands.is_empty() {
            chat::send_text_msg(
                &self.ctx,
                chat_id,
                format!("Sorry, I can't understand your request."),
            )
            .await?;
            return Ok(());
        }

        let result = self.zuk.run_commands(commands, &mut streams, Some(&user));
        let outputs = match result {
            Ok(output) => output,
            Err(errors) => errors,
        };

        for output in outputs {
            self.render_output(chat_id, output).await?;
        }

        Ok(())
    }

    async fn render_output(&self, chat_id: ChatId, output: Output) -> Result<()> {
        for block in output.blocks {
            self.render_block(chat_id, block).await?;
        }
        Ok(())
    }

    async fn render_block(&self, chat_id: ChatId, block: Block) -> Result<()> {
        match block {
            Block::Comment(comment) => {
                chat::send_text_msg(&self.ctx, chat_id, comment.text).await?;
            }
            Block::Data(data) => {
                if let Ok(text) = str::from_utf8(&data.data) {
                    let mut msg = Message::new(Viewtype::Text);
                    msg.set_text(Some(text.to_string()));
                    chat::send_msg(&self.ctx, chat_id, &mut msg).await?;
                } else {
                    let mut file = NamedTempFile::new()?;
                    file.write_all(&data.data)?;
                    file.flush()?;
                    let mut msg = Message::new(Viewtype::File);
                    let media_type = data.media_type.to_string();
                    msg.set_file(file.path().to_str().unwrap(), Some(&media_type));
                    chat::send_msg(&self.ctx, chat_id, &mut msg).await?;
                }
            }
            _ => {
                chat::send_text_msg(&self.ctx, chat_id, "[unimplemented]".to_string()).await?;
            }
        }
        Ok(())
    }
}
