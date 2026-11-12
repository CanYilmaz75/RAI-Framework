//! Der Teil des Treibers, der mit dem Modell spricht: Antwort lesen, Abbrüche neu anfragen,
//! alten Verlauf zusammenfassen lassen.

use can_core::{Assembler, Chunk, Message, ProviderError, Request, ToolOutput};
use futures::StreamExt;

use crate::driver::Driver;
use crate::exec;
use crate::step::Input;

/// So oft wird ein mittendrin abgebrochener Antwort-Strom neu angefragt.
const MAX_STREAM_RESTARTS: u32 = 2;

fn is_transient(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Network(_) | ProviderError::Overloaded | ProviderError::RateLimited { .. }
    )
}

impl Driver {
    /// Fragt das Modell. Bricht der Strom mittendrin mit einem vorübergehenden Fehler ab, wird
    /// die Teilantwort verworfen und der Schritt neu angefragt.
    pub(crate) async fn call_model(&mut self, request: Request) -> Input {
        let mut restarts = 0;
        loop {
            match self.stream_once(request.clone()).await {
                Err((error, true)) if restarts < MAX_STREAM_RESTARTS && is_transient(&error) => {
                    tracing::warn!(%error, "Modellantwort brach ab; der Schritt wird neu angefragt");
                    restarts += 1;
                }
                Err((error, _)) => return Input::ModelFailed(error),
                Ok(input) => return input,
            }
        }
    }

    /// Ein Versuch. Im Fehlerfall sagt der zweite Wert, ob schon Chunks angekommen waren.
    async fn stream_once(&mut self, request: Request) -> Result<Input, (ProviderError, bool)> {
        let mut stream = self
            .agent
            .provider
            .stream(request)
            .await
            .map_err(|error| (error, false))?;
        let mut assembler = Assembler::default();
        let mut received = false;
        while let Some(item) = stream.next().await {
            let chunk = item.map_err(|error| (error, received))?;
            received = true;
            if let Chunk::TextDelta { text } = &chunk {
                self.send_delta(text);
            }
            assembler.push(chunk).map_err(|error| (error, false))?;
        }
        let (message, usage, stop) = assembler.finish().map_err(|error| (error, false))?;
        Ok(Input::ModelDone {
            message,
            usage,
            stop,
        })
    }

    /// Lässt das Modell den alten Verlauf zusammenfassen; Deltas gehen dabei nicht an den Kanal.
    pub(crate) async fn summarize(&self, up_to_seq: u64, request: Request) -> Input {
        let mut stream = match self.agent.provider.stream(request).await {
            Ok(stream) => stream,
            Err(error) => return Input::CompactFailed(error.to_string()),
        };
        let mut assembler = Assembler::default();
        while let Some(item) = stream.next().await {
            if let Err(error) = item.and_then(|chunk| assembler.push(chunk)) {
                return Input::CompactFailed(error.to_string());
            }
        }
        match assembler.finish() {
            Ok((message, ..)) => Input::Compacted {
                up_to_seq,
                summary: Message::assistant(message.text()),
            },
            Err(error) => Input::CompactFailed(error.to_string()),
        }
    }

    /// Kürzt eine zu große Ausgabe für das Modell. Die volle Ausgabe geht als Blob in den Store;
    /// das Ergebnis nennt ihre Inhaltsadresse.
    pub(crate) async fn shorten(&self, output: ToolOutput, max_bytes: usize) -> ToolOutput {
        if exec::text_bytes(&output) <= max_bytes {
            return output;
        }
        let full = self.agent.redactor.text(&output.as_text()).into_bytes();
        let size = full.len();
        let mut shortened = exec::truncate(output, max_bytes);
        let note = match self.agent.store.put_blob(full).await {
            Ok(digest) => format!("\n[Volle Ausgabe: {size} Bytes, im Store als Blob {digest}]"),
            Err(error) => format!("\n[Volle Ausgabe: {size} Bytes, nicht gespeichert: {error}]"),
        };
        shortened.0.push(can_core::OutputBlock::Text(note));
        shortened
    }
}
