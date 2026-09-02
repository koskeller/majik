//! Base URLs and other per-provider constants.

pub mod fal {
    pub const BASE_URL: &str = "https://fal.run";
    pub const QUEUE_BASE_URL: &str = "https://queue.fal.run";
    /// fal's file storage, for inputs too big to inline as a data URI (a whole video).
    pub const REST_BASE_URL: &str = "https://rest.fal.ai";
    pub const ELEVENLABS_V3_MONOLOGUE_ENDPOINT: &str = "fal-ai/elevenlabs/tts/eleven-v3";
    pub const ELEVENLABS_V3_DIALOGUE_ENDPOINT: &str = "fal-ai/elevenlabs/text-to-dialogue/eleven-v3";
    pub const GEMINI_TTS_ENDPOINT: &str = "fal-ai/gemini-tts";
    /// fal's LLM router; `TEXT_MODEL` is one of the model ids it accepts.
    pub const ANY_LLM_ENDPOINT: &str = "fal-ai/any-llm";
    /// See the note on `openrouter::TEXT_MODEL`. fal refuses a reasoning model outright unless
    /// reasoning is enabled, which this is not, and `any-llm`'s model list stops at the Claude 4.5
    /// generation (checked 2026-09-02), so this is the largest Claude it routes.
    pub const TEXT_MODEL: &str = "anthropic/claude-sonnet-4.5";
}

pub mod openrouter {
    pub const BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
    pub const HTTP_REFERER: &str = "https://majik.app";
    pub const TITLE: &str = "Majik";
    /// Rewriting a prompt is a quick task the user waits on with the composer frozen, so thinking
    /// must be off: reasoning tokens count against the budget and leave an empty completion, and
    /// the latency risks the 30 s deadline. Claude models think only when asked to, and a request
    /// that says nothing about reasoning doesn't ask. (Tested live on Haiku 4.5;
    /// `openai/gpt-5-mini` returned nothing here and fal rejected it outright.) Sonnet: the edit is
    /// meant to be light, and a smaller model tends to rewrite instead.
    pub const TEXT_MODEL: &str = "anthropic/claude-sonnet-5";
}

pub mod replicate {
    pub const BASE_URL: &str = "https://api.replicate.com/v1";
    /// See the note on `openrouter::TEXT_MODEL`. An official model, so it is called by slug rather
    /// than pinned to a version. Its schema has an `effort` knob whose `low` setting turns thinking
    /// off, which `complete_text` sends.
    pub const TEXT_MODEL: &str = "anthropic/claude-sonnet-5";
    /// philz1337x/clarity-upscaler
    pub const UPSCALE_VERSION: &str = "dfad41707589d68ecdccd1dfa600d55a208f9310748e44bfe35b4a6291453d5e";
    /// 851-labs/background-remover
    pub const REMOVE_BACKGROUND_VERSION: &str = "a029dff38972b5fda4ec5d75d7d1cd25aeff621d2cf4946a41055d7db66b80bc";
}
