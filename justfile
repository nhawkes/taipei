default: dev

# Run the blog. The server builds the `blog-app` crate to ONE wasip2 component itself
# (membrane SSR + jco browser bundle) and hot-reloads on change.
dev:
    cargo run -p blog-server
