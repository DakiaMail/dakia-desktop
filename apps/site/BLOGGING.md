# Publishing a blog post

The blog is a small source-controlled content collection. To publish a new post:

1. Add an object to `src/content/posts.ts` with a unique `slug`, ISO `date`, title, description, category, read time, and body sections.
2. Run `npm run build` from `apps/site`.
3. Deploy the generated `dist` directory through the chosen website host.

The post will automatically appear at `/blog` and at `/blog/<slug>`.
