use diesel::{self, ExpressionMethods, QueryDsl, RunQueryDsl};

use lists::List;
use posts::Post;
use schema::{posts, timeline, timeline_definition};
use std::ops::Deref;
use {Connection, Error, PlumeRocket, Result};

pub(crate) mod query;

use self::query::{Kind, QueryError, TimelineQuery};

#[derive(Clone, Debug, PartialEq, Queryable, Identifiable, AsChangeset)]
#[table_name = "timeline_definition"]
pub struct Timeline {
    pub id: i32,
    pub user_id: Option<i32>,
    pub name: String,
    pub query: String,
}

#[derive(Default, Insertable)]
#[table_name = "timeline_definition"]
pub struct NewTimeline {
    user_id: Option<i32>,
    name: String,
    query: String,
}

#[derive(Default, Insertable)]
#[table_name = "timeline"]
struct TimelineEntry {
    pub post_id: i32,
    pub timeline_id: i32,
}

impl Timeline {
    insert!(timeline_definition, NewTimeline);
    get!(timeline_definition);

    pub fn find_by_name(conn: &Connection, user_id: Option<i32>, name: &str) -> Result<Self> {
        if let Some(user_id) = user_id {
            timeline_definition::table
                .filter(timeline_definition::user_id.eq(user_id))
                .filter(timeline_definition::name.eq(name))
                .first(conn)
                .map_err(Error::from)
        } else {
            timeline_definition::table
                .filter(timeline_definition::user_id.is_null())
                .filter(timeline_definition::name.eq(name))
                .first(conn)
                .map_err(Error::from)
        }
    }

    pub fn list_for_user(conn: &Connection, user_id: Option<i32>) -> Result<Vec<Self>> {
        if let Some(user_id) = user_id {
            timeline_definition::table
                .filter(timeline_definition::user_id.eq(user_id))
                .load::<Self>(conn)
                .map_err(Error::from)
        } else {
            timeline_definition::table
                .filter(timeline_definition::user_id.is_null())
                .load::<Self>(conn)
                .map_err(Error::from)
        }
    }

    pub fn new_for_user(
        conn: &Connection,
        user_id: i32,
        name: String,
        query_string: String,
    ) -> Result<Timeline> {
        {
            let query = TimelineQuery::parse(&query_string)?; // verify the query is valid
            if let Some(err) =
                query
                    .list_used_lists()
                    .into_iter()
                    .find_map(|(name, kind)| {
                        let list = List::find_by_name(conn, Some(user_id), &name)
                            .map(|l| l.kind() == kind);
                        match list {
                            Ok(true) => None,
                            Ok(false) => Some(Error::TimelineQuery(QueryError::RuntimeError(
                                format!("list '{}' has the wrong type for this usage", name),
                            ))),
                            Err(_) => Some(Error::TimelineQuery(QueryError::RuntimeError(
                                format!("list '{}' was not found", name),
                            ))),
                        }
                    })
            {
                Err(err)?;
            }
        }
        Self::insert(
            conn,
            NewTimeline {
                user_id: Some(user_id),
                name,
                query: query_string,
            },
        )
    }

    pub fn new_for_instance(
        conn: &Connection,
        name: String,
        query_string: String,
    ) -> Result<Timeline> {
        {
            let query = TimelineQuery::parse(&query_string)?; // verify the query is valid
            if let Some(err) =
                query
                    .list_used_lists()
                    .into_iter()
                    .find_map(|(name, kind)| {
                        let list = List::find_by_name(conn, None, &name).map(|l| l.kind() == kind);
                        match list {
                            Ok(true) => None,
                            Ok(false) => Some(Error::TimelineQuery(QueryError::RuntimeError(
                                format!("list '{}' has the wrong type for this usage", name),
                            ))),
                            Err(_) => Some(Error::TimelineQuery(QueryError::RuntimeError(
                                format!("list '{}' was not found", name),
                            ))),
                        }
                    })
            {
                Err(err)?;
            }
        }
        Self::insert(
            conn,
            NewTimeline {
                user_id: None,
                name,
                query: query_string,
            },
        )
    }

    pub fn update(&self, conn: &Connection) -> Result<Self> {
        diesel::update(self).set(self).execute(conn)?;
        let timeline = Self::get(conn, self.id)?;
        Ok(timeline)
    }

    pub fn delete(&self, conn: &Connection) -> Result<()> {
        diesel::delete(self)
            .execute(conn)
            .map(|_| ())
            .map_err(Error::from)
    }

    pub fn get_latest(&self, conn: &Connection, count: i32) -> Result<Vec<Post>> {
        self.get_page(conn, (0, count))
    }

    pub fn get_page(&self, conn: &Connection, (min, max): (i32, i32)) -> Result<Vec<Post>> {
        timeline::table
            .filter(timeline::timeline_id.eq(self.id))
            .inner_join(posts::table)
            .order(posts::creation_date.desc())
            .offset(min.into())
            .limit((max - min).into())
            .select(posts::all_columns)
            .load::<Post>(conn)
            .map_err(Error::from)
    }

    pub fn add_to_all_timelines(rocket: &PlumeRocket, post: &Post, kind: Kind) -> Result<()> {
        let timelines = timeline_definition::table
            .load::<Self>(rocket.conn.deref())
            .map_err(Error::from)?;

        for t in timelines {
            if t.matches(rocket, post, kind)? {
                t.add_post(&rocket.conn, post)?;
            }
        }
        Ok(())
    }

    pub fn add_post(&self, conn: &Connection, post: &Post) -> Result<()> {
        diesel::insert_into(timeline::table)
            .values(TimelineEntry {
                post_id: post.id,
                timeline_id: self.id,
            })
            .execute(conn)?;
        Ok(())
    }

    pub fn matches(&self, rocket: &PlumeRocket, post: &Post, kind: Kind) -> Result<bool> {
        let query = TimelineQuery::parse(&self.query)?;
        query.matches(rocket, self, post, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blogs::tests as blogTests;
    use diesel::Connection;
    use lists::ListType;
    use post_authors::{NewPostAuthor, PostAuthor};
    use posts::NewPost;
    use safe_string::SafeString;
    use tags::Tag;
    use tests::{db, rockets};
    use users::tests as userTests;

    #[test]
    fn test_timeline() {
        let conn = &db();
        conn.test_transaction::<_, (), _>(|| {
            let users = userTests::fill_database(conn);

            let mut tl1_u1 = Timeline::new_for_user(
                conn,
                users[0].id,
                "my timeline".to_owned(),
                "all".to_owned(),
            )
            .unwrap();
            List::new(conn, "languages I speak", Some(&users[1]), ListType::Prefix).unwrap();
            let tl2_u1 = Timeline::new_for_user(
                conn,
                users[0].id,
                "another timeline".to_owned(),
                "followed".to_owned(),
            )
            .unwrap();
            let tl1_u2 = Timeline::new_for_user(
                conn,
                users[1].id,
                "english posts".to_owned(),
                "lang in \"languages I speak\"".to_owned(),
            )
            .unwrap();
            let tl1_instance = Timeline::new_for_instance(
                conn,
                "english posts".to_owned(),
                "license in [cc]".to_owned(),
            )
            .unwrap();

            assert_eq!(tl1_u1, Timeline::get(conn, tl1_u1.id).unwrap());
            assert_eq!(
                tl2_u1,
                Timeline::find_by_name(conn, Some(users[0].id), "another timeline").unwrap()
            );
            assert_eq!(
                tl1_instance,
                Timeline::find_by_name(conn, None, "english posts").unwrap()
            );

            let tl_u1 = Timeline::list_for_user(conn, Some(users[0].id)).unwrap();
            assert_eq!(2, tl_u1.len());
            if tl1_u1.id == tl_u1[0].id {
                assert_eq!(tl1_u1, tl_u1[0]);
                assert_eq!(tl2_u1, tl_u1[1]);
            } else {
                assert_eq!(tl2_u1, tl_u1[0]);
                assert_eq!(tl1_u1, tl_u1[1]);
            }

            let tl_instance = Timeline::list_for_user(conn, None).unwrap();
            assert_eq!(1, tl_instance.len());
            assert_eq!(tl1_instance, tl_instance[0]);

            tl1_u1.name = "My Super TL".to_owned();
            let new_tl1_u2 = tl1_u2.update(conn).unwrap();

            let tl_u2 = Timeline::list_for_user(conn, Some(users[1].id)).unwrap();
            assert_eq!(1, tl_u2.len());
            assert_eq!(new_tl1_u2, tl_u2[0]);

            Ok(())
        });
    }

    #[test]
    fn test_timeline_creation_error() {
        let conn = &db();
        conn.test_transaction::<_, (), _>(|| {
            let users = userTests::fill_database(conn);

            assert!(Timeline::new_for_user(
                conn,
                users[0].id,
                "my timeline".to_owned(),
                "invalid keyword".to_owned(),
            )
            .is_err());
            assert!(Timeline::new_for_instance(
                conn,
                "my timeline".to_owned(),
                "invalid keyword".to_owned(),
            )
            .is_err());

            assert!(Timeline::new_for_user(
                conn,
                users[0].id,
                "my timeline".to_owned(),
                "author in non_existant_list".to_owned(),
            )
            .is_err());
            assert!(Timeline::new_for_instance(
                conn,
                "my timeline".to_owned(),
                "lang in dont-exist".to_owned(),
            )
            .is_err());

            List::new(conn, "friends", Some(&users[0]), ListType::User).unwrap();
            List::new(conn, "idk", None, ListType::Blog).unwrap();

            assert!(Timeline::new_for_user(
                conn,
                users[0].id,
                "my timeline".to_owned(),
                "blog in friends".to_owned(),
            )
            .is_err());
            assert!(Timeline::new_for_instance(
                conn,
                "my timeline".to_owned(),
                "not author in idk".to_owned(),
            )
            .is_err());

            Ok(())
        });
    }

    #[test]
    fn test_simple_match() {
        let r = &rockets();
        let conn = &r.conn;
        conn.test_transaction::<_, (), _>(|| {
            let (users, blogs) = blogTests::fill_database(conn);

            let gnu_tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "GNU timeline".to_owned(),
                "license in [AGPL, LGPL, GPL]".to_owned(),
            )
            .unwrap();

            let gnu_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug".to_string(),
                    title: "About Linux".to_string(),
                    content: SafeString::new("you must say GNU/Linux, not Linux!!!"),
                    published: true,
                    license: "GPL".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "you must say GNU/Linux, not Linux!!!".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();
            assert!(gnu_tl.matches(r, &gnu_post, Kind::Original).unwrap());

            let non_free_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug2".to_string(),
                    title: "Private is bad".to_string(),
                    content: SafeString::new("so is Microsoft"),
                    published: true,
                    license: "all right reserved".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "so is Microsoft".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();
            assert!(!gnu_tl.matches(r, &non_free_post, Kind::Original).unwrap());

            Ok(())
        });
    }

    #[test]
    fn test_add_to_all_timelines() {
        let r = &rockets();
        let conn = &r.conn;
        conn.test_transaction::<_, (), _>(|| {
            let (users, blogs) = blogTests::fill_database(conn);

            let gnu_tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "GNU timeline".to_owned(),
                "license in [AGPL, LGPL, GPL]".to_owned(),
            )
            .unwrap();
            let non_gnu_tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Stallman disapproved timeline".to_owned(),
                "not license in [AGPL, LGPL, GPL]".to_owned(),
            )
            .unwrap();

            let gnu_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug".to_string(),
                    title: "About Linux".to_string(),
                    content: SafeString::new("you must say GNU/Linux, not Linux!!!"),
                    published: true,
                    license: "GPL".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "you must say GNU/Linux, not Linux!!!".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();

            let non_free_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug2".to_string(),
                    title: "Private is bad".to_string(),
                    content: SafeString::new("so is Microsoft"),
                    published: true,
                    license: "all right reserved".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "so is Microsoft".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();

            Timeline::add_to_all_timelines(r, &gnu_post, Kind::Original).unwrap();
            Timeline::add_to_all_timelines(r, &non_free_post, Kind::Original).unwrap();

            let res = gnu_tl.get_latest(conn, 2).unwrap();
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].id, gnu_post.id);
            let res = non_gnu_tl.get_latest(conn, 2).unwrap();
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].id, non_free_post.id);

            Ok(())
        });
    }

    #[test]
    fn test_matches_lists_direct() {
        let r = &rockets();
        let conn = &r.conn;
        conn.test_transaction::<_, (), _>(|| {
            let (users, blogs) = blogTests::fill_database(conn);

            let gnu_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug".to_string(),
                    title: "About Linux".to_string(),
                    content: SafeString::new("you must say GNU/Linux, not Linux!!!"),
                    published: true,
                    license: "GPL".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "you must say GNU/Linux, not Linux!!!".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();
            gnu_post
                .update_tags(conn, vec![Tag::build_activity("free".to_owned()).unwrap()])
                .unwrap();
            PostAuthor::insert(
                conn,
                NewPostAuthor {
                    post_id: gnu_post.id,
                    author_id: blogs[0].list_authors(conn).unwrap()[0].id,
                },
            )
            .unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "blog timeline".to_owned(),
                format!("blog in [{}]", blogs[0].fqn),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "blog timeline".to_owned(),
                "blog in [no_one@nowhere]".to_owned(),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "author timeline".to_owned(),
                format!(
                    "author in [{}]",
                    blogs[0].list_authors(conn).unwrap()[0].fqn
                ),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "author timeline".to_owned(),
                format!("author in [{}]", users[2].fqn),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            assert!(tl.matches(r, &gnu_post, Kind::Reshare(&users[2])).unwrap());
            assert!(!tl.matches(r, &gnu_post, Kind::Like(&users[2])).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "author timeline".to_owned(),
                format!(
                    "author in [{}] include likes exclude reshares",
                    users[2].fqn
                ),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            assert!(!tl.matches(r, &gnu_post, Kind::Reshare(&users[2])).unwrap());
            assert!(tl.matches(r, &gnu_post, Kind::Like(&users[2])).unwrap());
            tl.delete(conn).unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "tag timeline".to_owned(),
                "tags in [free]".to_owned(),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "tag timeline".to_owned(),
                "tags in [private]".to_owned(),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();

            Ok(())
        });
    }

    /*
    #[test]
    fn test_matches_lists_saved() {
        let r = &rockets();
        let conn = &r.conn;
        conn.test_transaction::<_, (), _>(|| {
            let (users, blogs) = blogTests::fill_database(conn);

            let gnu_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug".to_string(),
                    title: "About Linux".to_string(),
                    content: SafeString::new("you must say GNU/Linux, not Linux!!!"),
                    published: true,
                    license: "GPL".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "".to_string(),
                    source: "you must say GNU/Linux, not Linux!!!".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();
            gnu_post.update_tags(conn, vec![Tag::build_activity("free".to_owned()).unwrap()]).unwrap();
            PostAuthor::insert(conn, NewPostAuthor {post_id: gnu_post.id, author_id: blogs[0].list_authors(conn).unwrap()[0].id}).unwrap();

            unimplemented!();

            Ok(())
        });
    }*/

    #[test]
    fn test_matches_keyword() {
        let r = &rockets();
        let conn = &r.conn;
        conn.test_transaction::<_, (), _>(|| {
            let (users, blogs) = blogTests::fill_database(conn);

            let gnu_post = Post::insert(
                conn,
                NewPost {
                    blog_id: blogs[0].id,
                    slug: "slug".to_string(),
                    title: "About Linux".to_string(),
                    content: SafeString::new("you must say GNU/Linux, not Linux!!!"),
                    published: true,
                    license: "GPL".to_string(),
                    ap_url: "".to_string(),
                    creation_date: None,
                    subtitle: "Stallman is our god".to_string(),
                    source: "you must say GNU/Linux, not Linux!!!".to_string(),
                    cover_id: None,
                },
                &r.searcher,
            )
            .unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Linux title".to_owned(),
                "title contains Linux".to_owned(),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Microsoft title".to_owned(),
                "title contains Microsoft".to_owned(),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Linux subtitle".to_owned(),
                "subtitle contains Stallman".to_owned(),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Microsoft subtitle".to_owned(),
                "subtitle contains Nadella".to_owned(),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();

            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Linux content".to_owned(),
                "content contains Linux".to_owned(),
            )
            .unwrap();
            assert!(tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();
            let tl = Timeline::new_for_user(
                conn,
                users[0].id,
                "Microsoft content".to_owned(),
                "subtitle contains Windows".to_owned(),
            )
            .unwrap();
            assert!(!tl.matches(r, &gnu_post, Kind::Original).unwrap());
            tl.delete(conn).unwrap();

            Ok(())
        });
    }
}