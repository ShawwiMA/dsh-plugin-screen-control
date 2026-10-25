/**
 * IPC 协议类型 —— 与 `docs/IPC.md` 一一对应。
 *
 * 改这里的任何字段名,必须同步改 docs/IPC.md 和 rust-engine/src/protocol.rs。
 * 三处不一致时以 docs/IPC.md 为准。
 */
/** 引擎侧返回的错误,`code` 直接来自协议。 */
export class EngineError extends Error {
    code;
    constructor(code, message) {
        super(message);
        this.name = 'EngineError';
        this.code = code;
    }
}
//# sourceMappingURL=types.js.map