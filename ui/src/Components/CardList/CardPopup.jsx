import { useCallback, useEffect, useState } from "react";
import { FontAwesomeIcon } from "@fortawesome/react-fontawesome";
import { Link } from "react-router-dom";
import { connect } from "react-redux";

import TruncText from "../../Helpers/TruncText.jsx";
import IMDbLogo from "../../assets/imdb.png";
import PlayButton from "../PlayButton.jsx";

import "./CardPopup.scss";

function CardPopup(props) {
  const [overflowing, setOverflowing] = useState(false);
  const [mediaVersions, setMediaVersions] = useState([]);
  const [direction, setDirection] = useState("card-popup-right");

  const { setHovering } = props;

  const onAnimationEnd = useCallback(e => {
    if (e.animationName !== "CardPopupHide") return;
    setHovering(false);
  }, [setHovering]);

  useEffect(() => {
    const { x, width } = props.popup.current.getBoundingClientRect();
    const overflowing = (x + width > window.innerWidth - 5);

    if (!overflowing) return;

    setOverflowing(true);
    setDirection("card-popup-left");
  }, [props.popup, overflowing]);

  const {
    id,
    name,
    rating,
    description,
    genres,
    year,
    duration
  } = props.data;

  const { auth } = props;
  const { token } = auth;

  // to get file versions
  useEffect(() => {
    // note: quickly coded
    (async () => {
      const config = {
        headers: {
          "authorization": token
        }
      };

      const res = await fetch(`//${window.host}:8000/api/v1/media/${id}/info`, config);

      if (res.status !== 200) return;

      const payload = await res.json();

      if (payload.error) return;

      if (payload.seasons) {
        setMediaVersions(
          payload.seasons[0].episodes[0].versions
        );
      } else {
        setMediaVersions(payload.versions);
      }
    })();
  }, [id, token]);

  const length = {
    hh: ("0" + Math.floor(duration / 3600)).slice(-2),
    mm: ("0" + Math.floor((duration % 3600) / 60)).slice(-2),
    ss: ("0" + Math.floor((duration % 3600) % 60)).slice(-2)
  };

  if (genres.length > 3) {
    genres.length = 3
  }

  return (
    <div
      className={direction}
      ref={props.popup}
      onAnimationEnd={onAnimationEnd}
    >
      <div className="clipped"/>
      <div className="contentWrapper">
        <section className="header">
          <h2><TruncText content={name} max={8}/></h2>
          <div className="rating">
            <p>{rating || 0}</p>
            <img alt="imdb" src={IMDbLogo}/>
          </div>
        </section>
        <section className="separator"/>
        <section className="description">
          {description !== null && description.length > 0
            ? <p><TruncText content={description} max={21}/></p>
            : <p>No description found.</p>
          }
        </section>
        <section className="tags">
          <Link to={`/search?year=${year}`}>{year}</Link>
          <FontAwesomeIcon icon="circle"/>
          <div className="genres">
            {genres.map((genre, i) => (
              <Link
                to={`/search?genre=${encodeURIComponent(genre)}`}
                key={i}
              >
                {genre}
              </Link>
            ))}
          </div>
        </section>
        <section className="separator"/>
        <section className="footer">
          <div className="length">
            <p>{length.hh}:{length.mm}:{length.ss}</p>
            <p>HH MM SS</p>
          </div>
          <PlayButton mediaID={id} versions={mediaVersions}/>
        </section>
      </div>
    </div>
  );
}

const mapStateToProps = (state) => ({
  auth: state.auth
});

const mapActionstoProps = {};

export default connect(mapStateToProps, mapActionstoProps)(CardPopup);
