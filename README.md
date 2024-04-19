# Easy threadpool

A simple threadpool panic tolerant threadpool which can efficiently work and wait for multiple tasks.

## Why was this made

This threadpool is nothing special, I admit I didn't really do my homework on other available options and as such this is almost certainly the worse option compared to the alternatives listed below. I wanted to make something cool, and I needed a threadpool a few months back, and so I made one. Besides that I have a personal thing against lots of dependencies so this threadpool does not have _any_ dependencies aside from the standard library.

## Cool features

- No dependencies aside from the standard library
- Completely panic tolerant (for as far as I've been able to test). No matter what a job does the threadpool won't crash
- Efficient functions that wait until all jobs are finished through synchronization primitives
- The ability to have multiple 'states' execute jobs on the same threads, aka you can have one state that sends jobs that panic which then do not affect the other states

## Want to haves

- I would like to make the threadpool a little more customizable with custom thread names/ stack sizes
- I would like to implement a way to invalidate/ clear tasks so they won't be ran taking up CPU time
- If I ever find the time I'd love to look into dynamically giving threads stack sizes, a little like go threads
- If I ever go mentally insane I'd like to look into more efficiently scheduling tasks maybe based on some priority or giving different instances of the threadpool equal CPU time instead of just executing jobs one after the other and through that make sharing one pool more usable and nice.

## Usage

Add this to the `Cargo.toml` to use the crate:

```toml
[dependencies]
easy-threadpool = "0.1.0"
```

Afterwards use the library like this:

```rust
use std::error::Error;
use easy_threadpool::ThreadPoolBuilder;

fn main() -> Result<(), Box<dyn Error>> {

    fn job() {
        println!("Hello world!");
    }

    let builder = ThreadPoolBuilder::with_max_threads()?;
    let pool = builder.build()?;

    for _ in 0..10 {
        pool.send_job(job);
    }

    assert!(pool.wait_until_finished().is_ok());
    Ok(())
}
```

For more examples, look at the documentation in the source [here](https://github.com/NicoElbers/easy_threadpool/blob/4348087c8ceb9774290c17fe0ec8662db0ff268f/src/lib.rs#L11).

## MSRV

This crate currently works with rust version 1.72.0 or later. But this is in no way guaranteed as development continues.

## Similar libraries

- [rust-threadpool](https://github.com/rust-threadpool/rust-threadpool/tree/master)
- [rayon (`rayon::ThreadPool`)](https://docs.rs/rayon/*/rayon/struct.ThreadPool.html)
- [rust-scoped-pool](http://github.com/reem/rust-scoped-pool)
- [scoped-threadpool-rs](https://github.com/Kimundi/scoped-threadpool-rs)
- [crossbeam](https://github.com/aturon/crossbeam)

## License

Licensed under the MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT).
