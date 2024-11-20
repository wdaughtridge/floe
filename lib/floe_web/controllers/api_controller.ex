defmodule FloeWeb.ApiController do
  use FloeWeb, :controller

  def whip(conn, _params) do
    {:ok, sdp_offer, conn} = Plug.Conn.read_body(conn)

    {:ok, link} = Floe.SFU.start_link()
    {:ok, sdp_answer} = Floe.SFU.put_new_whep_client(sdp_offer, link)

    conn
    |> Plug.Conn.put_resp_header("location", "/resource/12345")
    |> Plug.Conn.put_resp_header("content-type", "application/sdp")
    |> Plug.Conn.send_resp(201, sdp_answer)
  end
end
