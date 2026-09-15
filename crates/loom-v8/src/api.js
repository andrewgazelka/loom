(() => {
    'use strict';
    const nativePerform = globalThis.__loomPerform;
    const nativeEncode = globalThis.__loomEncode;
    const stringify = JSON.stringify;
    const encode = value => nativeEncode(stringify(value));
    const decode = globalThis.__loomDecode;
    for (const name of ['__loomPerform', '__loomEncode', '__loomDecode']) {
        if (typeof globalThis[name] !== 'function' || !delete globalThis[name]) {
            throw new Error('Loom native bridge was not installed privately');
        }
    }
    const freeze = Object.freeze;
    const isArray = Array.isArray;
    const isInteger = Number.isInteger;
    const perform = freeze(async (op, args) => nativePerform({op, args}));
    const bytes = value => {
        if (!isArray(value)) throw new TypeError('Expected an array of bytes');
        const copy = [];
        for (let index = 0; index < value.length; index++) {
            const byte = value[index];
            if (!isInteger(byte) || byte < 0 || byte > 255) {
                throw new TypeError('Expected integer bytes from 0 to 255');
            }
            copy[index] = byte;
        }
        return freeze(copy);
    };
    const get = freeze(token => {
        // This wrapper conveys no authority: the host checks the capability
        // against the actor's persisted grants on every operation.
        const cap = bytes(token);
        return freeze({
            cap,
            send: freeze(value => perform('actor.send', {cap, msg: encode(value)})),
            sendBytes: freeze(value => perform('actor.send', {cap, msg: bytes(value)})),
            // Calls enqueue a request and return its reference. The reply is
            // delivered later as another actor message, never awaited here.
            call: freeze((value, {timeoutMs} = {}) =>
                perform('actor.call', {cap, msg: encode(value), timeout_ms: timeoutMs})),
            reply: freeze((reference, value) =>
                perform('actor.reply', {cap, reference, msg: encode(value)})),
            sendAfter: freeze((ms, value) =>
                perform('actor.send_after', {cap, ms, msg: encode(value)})),
            stop: freeze(reason => perform('actor.stop', {cap, reason})),
            toJSON: freeze(() => cap),
        });
    });
    const actors = freeze({
        get,
        accept: freeze(async token => {
            const ref = get(token);
            await perform('actor.accept', {cap: ref.cap});
            return ref;
        }),
        spawn: freeze(async spec => get(await perform('actor.spawn', spec))),
        self: freeze(async () => get(await perform('actor.self_cap', null))),
    });
    const messages = freeze({
        encode: freeze(encode),
        decode: freeze(decode),
        json: freeze(handler => {
            if (typeof handler !== 'function') throw new TypeError('Message handler must be a function');
            return freeze(messageBytes => handler(decode(messageBytes)));
        }),
    });
    const blobs = new WeakMap();
    const sqlCell = value => {
        if (value === null) return {type: 'null'};
        if (typeof value === 'string') return {type: 'text', value};
        if (typeof value === 'number' && Number.isFinite(value)) {
            if (isInteger(value) && !Number.isSafeInteger(value)) {
                throw new TypeError('SQL integer exceeds JavaScript safe integer range');
            }
            return {type: isInteger(value) ? 'integer' : 'real', value};
        }
        if (value && typeof value === 'object' && blobs.has(value)) {
            return {type: 'blob', value: blobs.get(value)};
        }
        throw new TypeError('SQL parameters must be null, strings, finite numbers, or loom.sql.blob(bytes)');
    };
    const fromCell = cell => {
        if (!cell || typeof cell !== 'object') throw new TypeError('Invalid SQL cell');
        switch (cell.type) {
            case 'null': return null;
            case 'text': if (typeof cell.value === 'string') return cell.value; break;
            case 'integer': if (Number.isSafeInteger(cell.value)) return cell.value; break;
            case 'real': if (typeof cell.value === 'number' && Number.isFinite(cell.value)) return cell.value; break;
            case 'blob': return bytes(cell.value);
        }
        throw new TypeError('Invalid or unsafe SQL cell');
    };
    const sql = async (query, params = []) => {
        if (typeof query !== 'string' || !isArray(params)) throw new TypeError('SQL requires query text and a parameter array');
        const result = await perform('sql', {sql: query, params: params.map(sqlCell)});
        if (!result || !isArray(result.columns) || !isArray(result.rows)) throw new TypeError('Invalid SQL rows');
        const seen = new Set();
        for (const column of result.columns) {
            if (typeof column !== 'string' || seen.has(column)) throw new TypeError('SQL requires unique column names; use aliases');
            seen.add(column);
        }
        return result.rows.map(cells => {
            if (!isArray(cells) || cells.length !== result.columns.length) throw new TypeError('Invalid SQL row width');
            const row = Object.create(null);
            for (let index = 0; index < cells.length; index++) row[result.columns[index]] = fromCell(cells[index]);
            return row;
        });
    };
    sql.blob = freeze(value => {
        const blob = freeze(Object.create(null));
        blobs.set(blob, bytes(value));
        return blob;
    });
    freeze(sql);
    Object.defineProperty(globalThis, 'loom', {
        value: freeze({
            perform,
            actors,
            messages,
            now: freeze(() => perform('now', null)),
            random: freeze(n => perform('random', {n})),
            sql,
        }),
        writable: false,
        configurable: false,
    });
})();
