import { getImagePath } from "astro-takumi";

export const onRequest = (context: any) => {
  const route = context.locals.starlightRoute;
  const ogImageUrl = getImagePath({ url: context.url, site: context.site });

  route.head.push({ tag: "meta", attrs: { property: "og:title", content: route.entry?.data?.title || "" } });
  route.head.push({ tag: "meta", attrs: { property: "og:type", content: "website" } });
  route.head.push({ tag: "meta", attrs: { property: "og:image", content: ogImageUrl } });
  route.head.push({ tag: "meta", attrs: { name: "twitter:card", content: "summary_large_image" } });
  route.head.push({ tag: "meta", attrs: { name: "twitter:image", content: ogImageUrl } });

  const desc = route.entry?.data?.description;
  if (desc) {
    route.head.push({ tag: "meta", attrs: { property: "og:description", content: desc } });
  }
};
