use anyhow::Result;
use clap::Parser;
use futures::future::join_all;
use lazy_regex::regex_replace_all;
use mediatype::{media_type, MediaType};
use serenity::async_trait;
use serenity::http::client::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::model::id::UserId;
use serenity::prelude::*;
use std::io::Cursor;
use std::str;
use std::sync::Arc;
use yozuk::Yozuk;
use yozuk_helper_filetype::get_file_extension;
use yozuk_sdk::prelude::*;

const MAX_FILE_SIZE: usize = 10485760;

struct Handler {
    user_id: UserId,
    yozuk: Arc<Yozuk>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if let Err(err) = handle_message(self, ctx, msg).await {
            println!("{err}");
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

async fn handle_message(handler: &Handler, ctx: Context, msg: Message) -> Result<()> {
    let echo = msg.author.id == handler.user_id;
    let dm = msg.guild_id.is_none();
    let mention = msg.mentions.iter().any(|user| user.id == handler.user_id);
    if !echo && (dm || mention) {
        let content = regex_replace_all!(
            r#"<@\d+>"#i,
            & msg.content,
            |_| String::new(),
        );

        let filesize = msg.attachments.iter().fold(0, |acc, x| acc + x.size);
        if filesize as usize > MAX_FILE_SIZE {
            msg.reply(&ctx.http, "Too large file input (10MiB max.)")
                .await?;
            return Ok(());
        }

        let attachments = join_all(msg.attachments.iter().map(|att| att.download())).await;
        let mut attachments = attachments
            .into_iter()
            .zip(&msg.attachments)
            .filter_map(|(data, att)| data.ok().map(|data| (data, att.content_type.as_ref())))
            .map(|(data, content_type)| {
                InputStream::new(
                    Cursor::new(data),
                    content_type
                        .and_then(|ty| MediaType::parse(ty).ok())
                        .unwrap_or(media_type!(APPLICATION / OCTET_STREAM)),
                )
            })
            .collect::<Vec<_>>();

        let tokens = Tokenizer::new().tokenize(&content);
        let commands = handler.yozuk.get_commands(&tokens, &attachments);
        if commands.is_empty() {
            msg.channel_id
                .send_message(&ctx.http, |m| {
                    m.content("Sorry, I can't understand your request.")
                        .add_embed(|f| {
                            f.field(
                                "Hint",
                                "Please refer [Documentation](https://docs.yozuk.com/) for available commands.",
                                true,
                            )
                        })
                        .reference_message(&msg)
                })
                .await?;
            return Ok(());
        }

        let user = UserContext {
            username: Some(msg.author.name.clone()),
            ..Default::default()
        };

        let result = handler
            .yozuk
            .run_commands(commands, &mut attachments, Some(&user));
        let outputs = match result {
            Ok(outputs) => outputs,
            Err(outputs) => outputs,
        };

        let mut content = vec![];
        let mut files = vec![];

        for output in outputs {
            for block in output.blocks {
                match block {
                    Block::Comment(comment) => {
                        content.push(comment.text);
                    }
                    Block::Data(data) => match str::from_utf8(&data.data) {
                        Ok(text) if text.len() <= 1024 => {
                            content.push(format!("```\n{}\n```", text));
                        }
                        _ => {
                            let filename = if data.file_name.is_empty() {
                                format!("data.{}", get_file_extension(&data.media_type))
                            } else {
                                data.file_name.clone()
                            };
                            files.push((data.data.clone(), filename));
                        }
                    },
                    _ => {}
                }
            }
        }

        msg.channel_id
            .send_message(&ctx.http, |m| {
                m.content(content.join("\n"))
                    .add_files(
                        files
                            .iter()
                            .map(|(data, name)| (data.as_ref(), name.as_str())),
                    )
                    .reference_message(&msg)
            })
            .await?;
    }
    Ok(())
}

#[derive(Parser)]
#[clap(author, version, about)]
pub struct Args {
    #[clap(long, env("DISCORD_TOKEN"), hide_env_values = true)]
    pub token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::try_parse()?;
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES;

    let http = Http::new(&args.token);
    let gateway = http.get_bot_gateway().await?;
    println!("{:?}", gateway);
    let user = http.get_current_user().await?;
    let yozuk = Arc::new(Yozuk::builder().build());

    let mut client = Client::builder(&args.token, intents)
        .event_handler(Handler {
            user_id: user.id,
            yozuk,
        })
        .await?;

    client.start().await?;
    Ok(())
}
