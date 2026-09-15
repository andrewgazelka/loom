(() => {
    'use strict';
    const nativePerform = globalThis.__loomPerform;
    const nativeEncode = globalThis.__loomEncode;
    const stringify = JSON.stringify;
    const encode = value => nativeEncode(stringify(value));
    const decode = globalThis.__loomDecode;
    const jsonHandler = globalThis.__loomJson;
    const actorHandler = globalThis.__loomActor;
    for (const name of ['__loomPerform', '__loomEncode', '__loomDecode', '__loomJson', '__loomActor']) {
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
    const text = value => {
        if (typeof value !== 'string') throw new TypeError('Expected text');
        return value;
    };
    const capability = value => bytes(isArray(value) ? value : value?.cap);
    const spawn = freeze(async spec => {
        if (!spec || typeof spec !== 'object' || isArray(spec)) throw new TypeError('Expected actor specification');
        const {behavior, init = null, ...options} = spec;
        const allowed = new Set(['durability', 'restart', 'shutdown', 'link', 'monitor', 'type']);
        for (const key of Object.keys(options)) {
            if (!allowed.has(key)) throw new TypeError(`Unknown actor option: ${key}`);
        }
        return get(await perform('actor.spawn', {behavior_hash: text(behavior), init: encode(init), ...options}));
    });
    const named = freeze(async name => get(await perform('actor.resolve', {name: text(name)})));
    const sender = freeze(async () => get(await perform('actor.sender_cap', null)));
    const actors = freeze({
        get,
        accept: freeze(async token => {
            const ref = get(token);
            await perform('actor.accept', {cap: ref.cap});
            return ref;
        }),
        spawn,
        named,
        sender,
        self: freeze(async () => get(await perform('actor.self_cap', null))),
    });
    const processRef = freeze(token => {
        const actor = get(token);
        return freeze({
            cap: actor.cap,
            write: freeze(data => actor.send({type: 'stdin', data: text(data)})),
            closeStdin: freeze(() => actor.send({type: 'close_stdin'})),
            cancel: freeze(() => actor.send({type: 'cancel'})),
            subscribe: freeze(subscriber => actor.send({type: 'subscribe', cap: capability(subscriber)})),
            toJSON: actor.toJSON,
        });
    });
    const processes = freeze({
        get: processRef,
        named: freeze(async name => processRef((await named(name)).cap)),
        spawn: freeze(async (name, options = {}) => {
            if (!options || typeof options !== 'object' || isArray(options)) throw new TypeError('Expected process options');
            for (const key of Object.keys(options)) {
                if (key !== 'subscriber') throw new TypeError(`Unknown process option: ${key}`);
            }
            const init = {process: text(name)};
            if (options.subscriber !== undefined) init.subscriber = capability(options.subscriber);
            // The host registers commands under names. Guests cannot supply
            // executable paths, arguments or environment through this API.
            const actor = await spawn({behavior: 'process-v1', init});
            return processRef(actor.cap);
        }),
    });
    const containers = freeze({
        spawn: freeze(async spec => {
            if (!spec || typeof spec !== 'object' || isArray(spec)) throw new TypeError('Expected container specification');
            const allowed = new Set(['image', 'command', 'args', 'env', 'limits', 'ttlMs', 'subscriber', 'network']);
            for (const key of Object.keys(spec)) {
                if (!allowed.has(key)) throw new TypeError(`Unknown container option: ${key}`);
            }
            const container = {image: text(spec.image)};
            if (container.image.length === 0) throw new TypeError('Container image must not be empty');
            if (spec.network !== undefined) {
                if (spec.network !== 'none' && spec.network !== 'bridge') throw new TypeError('Container network must be none or bridge');
                container.network = spec.network;
            }
            if (spec.command !== undefined) container.command = text(spec.command);
            if (spec.args !== undefined) {
                if (!isArray(spec.args)) throw new TypeError('Container args must be an array of strings');
                container.args = spec.args.map(text);
            }
            if (spec.env !== undefined) {
                if (!spec.env || typeof spec.env !== 'object' || isArray(spec.env)) throw new TypeError('Container env must be a string map');
                container.env = Object.create(null);
                for (const key of Object.keys(spec.env)) container.env[key] = text(spec.env[key]);
            }
            if (spec.limits !== undefined) {
                if (!spec.limits || typeof spec.limits !== 'object' || isArray(spec.limits)) throw new TypeError('Expected container limits');
                for (const key of Object.keys(spec.limits)) {
                    const value = spec.limits[key];
                    const valid = key === 'cpus' ? typeof value === 'number' && Number.isFinite(value) && value > 0
                        : (key === 'memoryMb' || key === 'pids') && Number.isSafeInteger(value) && value > 0;
                    if (!valid) throw new TypeError(`Invalid container limit: ${key}`);
                    container[key] = value;
                }
            }
            if (spec.ttlMs !== undefined) {
                if (!Number.isSafeInteger(spec.ttlMs) || spec.ttlMs <= 0) throw new TypeError('Container ttlMs must be a positive safe integer');
                container.ttlMs = spec.ttlMs;
            }
            const init = {container};
            if (spec.subscriber !== undefined) init.subscriber = capability(spec.subscriber);
            // Docker connection, mounts and privileges belong to the host.
            // Native validation applies tenant policy and the final limits.
            const actor = await spawn({behavior: 'container-v1', init});
            return processRef(actor.cap);
        }),
    });
    const websocketRef = freeze(token => {
        const actor = get(token);
        return freeze({
            cap: actor.cap,
            send: freeze(value => actor.send({type: 'send', data: {type: 'text', text: text(value)}})),
            sendBytes: freeze(value => actor.send({type: 'send', data: {type: 'binary', bytes: bytes(value)}})),
            close: freeze((code = 1000, reason = '') => {
                if (!isInteger(code) || code < 0 || code > 65535) {
                    throw new TypeError('WebSocket close code must be an unsigned 16-bit integer');
                }
                return actor.send({type: 'close', code, reason: text(reason)});
            }),
            toJSON: actor.toJSON,
        });
    });
    const websockets = freeze({
        get: websocketRef,
        sender: freeze(async () => websocketRef((await sender()).cap)),
        listen: freeze(async (config = {}) => {
            if (!config || typeof config !== 'object' || isArray(config)) throw new TypeError('Expected WebSocket options');
            const wire = {};
            for (const key of Object.keys(config)) {
                const value = config[key];
                if (key === 'maxConnections' && isInteger(value) && value >= 1 && value <= 4096) {
                    wire.max_connections = value;
                } else if (key === 'maxMessageBytes' && isInteger(value) && value >= 1 && value <= 16777216) {
                    wire.max_message_bytes = value;
                } else {
                    throw new TypeError(`Invalid WebSocket option: ${key}`);
                }
            }
            return get(await perform('actor.spawn_driver', {hash: 'websocket-v1', init: encode(wire)}));
        }),
    });
    const messages = freeze({
        encode: freeze(encode),
        decode: freeze(decode),
        json: freeze(handler => {
            if (typeof handler !== 'function') throw new TypeError('Message handler must be a function');
            return freeze(jsonHandler(handler));
        }),
    });
    const actor = freeze(handlers => {
        if (!handlers || typeof handlers !== 'object' || isArray(handlers)) throw new TypeError('Expected actor handlers');
        for (const key of Object.keys(handlers)) {
            if (key !== 'onStart' && key !== 'onMessage' && key !== 'onStop') throw new TypeError(`Unknown actor handler: ${key}`);
        }
        const {onStart, onMessage, onStop} = handlers;
        if (typeof onMessage !== 'function') throw new TypeError('onMessage must be a function');
        if (onStart !== undefined && typeof onStart !== 'function') throw new TypeError('onStart must be a function');
        if (onStop !== undefined && typeof onStop !== 'function') throw new TypeError('onStop must be a function');
        return freeze(actorHandler(onMessage, onStart, onStop));
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
            actor,
            actors,
            processes,
            containers,
            websockets,
            messages,
            now: freeze(() => perform('now', null)),
            random: freeze(n => perform('random', {n})),
            sql,
        }),
        writable: false,
        configurable: false,
    });
})();
