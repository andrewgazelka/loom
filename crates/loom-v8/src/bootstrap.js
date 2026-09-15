(() => {
    'use strict';

    // Backing-store allocations live outside the JavaScript heap limit. Keep
    // their constructors unreachable, including newer Float16Array support.
    // No instances are created here: an instance would expose its constructor.
    for (const name of [
        'ArrayBuffer', 'SharedArrayBuffer', 'DataView',
        'Int8Array', 'Uint8Array', 'Uint8ClampedArray',
        'Int16Array', 'Uint16Array', 'Int32Array', 'Uint32Array',
        'Float16Array', 'Float32Array', 'Float64Array',
        'BigInt64Array', 'BigUint64Array', 'WebAssembly', 'Atomics',
        'Date', 'Intl', 'Temporal', 'WeakRef', 'FinalizationRegistry',
        'setTimeout', 'clearTimeout', 'setInterval',
        'clearInterval', 'setImmediate', 'clearImmediate',
    ]) {
        Object.defineProperty(globalThis, name, {
            value: undefined,
            writable: false,
            configurable: false,
        });
    }
    // Time and randomness must pass through the actor's recorded effect path.
    Object.defineProperty(Math, 'random', {
        value: undefined,
        writable: false,
        configurable: false,
    });

})();
